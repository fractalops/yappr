//! Questions and final decisions in the writer's append-only feed.
use std::{collections::BTreeSet, fmt};

use anyhow::{bail, Result};
use iroh::EndpointId;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[cfg(not(wasm_browser))]
use crate::entry::Details;
use crate::entry::{Draft, Entry};
#[cfg(not(wasm_browser))]
use crate::store::Store;

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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unlock: bool,
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
    pub status: QuestionStatus,
    pub decision: Option<Decision>,
}

/// Where a question is. Only a pending question can get a decision or a
/// cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    Pending,
    Answered,
    Cancelled,
    Expired,
}
impl QuestionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}
impl fmt::Display for QuestionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
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

/// The choice record of an entry. A record that does not obey the rules of
/// its type is not a record, so no reader uses or shows its fields.
fn record(entry: &Entry) -> Option<Record> {
    let record: Record = serde_json::from_value(entry.extra.get(FIELD)?.clone()).ok()?;
    record.validate().ok()?;
    Some(record)
}

impl Record {
    fn validate(&self) -> Result<()> {
        match self {
            Record::Question { question } => question.validate(),
            Record::Decision { decision } => {
                if !valid_id(&decision.question_id) || !valid_id(&decision.submission_id) {
                    bail!("the decision has an ID that is not valid");
                }
                if !valid_value(&decision.value) {
                    bail!("the decision value is not valid");
                }
                Ok(())
            }
            Record::Cancelled { question_id } if valid_id(question_id) => Ok(()),
            Record::Cancelled { .. } => bail!("the question ID is not valid"),
        }
    }
}

/// The question that this entry asks.
pub fn question(entry: &Entry) -> Option<Question> {
    match record(entry)? {
        Record::Question { question } => Some(question),
        _ => None,
    }
}

/// The ID of the question that this entry closes with a decision or a
/// cancellation.
pub fn closed(entry: &Entry) -> Option<String> {
    match record(entry)? {
        Record::Decision { decision } => Some(decision.question_id),
        Record::Cancelled { question_id } => Some(question_id),
        Record::Question { .. } => None,
    }
}

/// The question ID of the question entry with this entry ID.
pub fn id_of_entry(entries: &[Entry], id: u64) -> Result<String> {
    let entry = entries
        .iter()
        .find(|e| e.id == id)
        .ok_or_else(|| ChoiceError::new("not_found", &format!("entry #{id} does not exist")))?;
    Ok(question(entry)
        .ok_or_else(|| ChoiceError::new("not_found", &format!("entry #{id} is not a question")))?
        .id)
}

#[cfg(not(wasm_browser))]
fn draft(record: Record, title: String, body: String, source: String, kind: &str) -> Result<Draft> {
    let mut extra = serde_json::Map::new();
    extra.insert(FIELD.into(), serde_json::to_value(record)?);
    Ok(Draft {
        source,
        kind: kind.into(),
        level: None,
        subject: None,
        title: Some(title),
        body,
        format: None,
        data: None,
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
            if !valid_value(&option.value) {
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
            level: None,
            subject: None,
            title: Some(self.title.clone()),
            body: self.body.clone(),
            format: None,
            data: None,
            extra: Default::default(),
        }
        .validate()?;
        Ok(())
    }
}

/// 32 random bytes as lowercase hexadecimal: the format of question,
/// submission, and invitation IDs.
pub fn random_id() -> String {
    data_encoding::HEXLOWER.encode(&iroh::SecretKey::generate().to_bytes())
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// 1 to 64 ASCII letters, digits, underscores, or hyphens: the format of a
/// choice value.
pub fn valid_value(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
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

#[cfg(not(wasm_browser))]
pub fn create(store: &Store, question: Question) -> Result<Question> {
    create_with(store, question, Details::default())
}

/// Create a question. Its entry also gets the optional `details`.
#[cfg(not(wasm_browser))]
pub fn create_with(store: &Store, question: Question, details: Details) -> Result<Question> {
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
        let mut entry = draft(
            Record::Question {
                question: question.clone(),
            },
            question.title.clone(),
            question.body.clone(),
            question.source.clone(),
            "question",
        )?;
        entry.level = details.level;
        entry.subject = details.subject;
        entry.format = details.format;
        entry.data = details.data;
        entry.validate()?;
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
                    status: QuestionStatus::Pending,
                    decision: None,
                });
            }
            Some(Record::Decision { decision }) => {
                if let Some(state) = self.pending(&decision.question_id) {
                    state.status = QuestionStatus::Answered;
                    state.decision = Some(decision);
                }
            }
            Some(Record::Cancelled { question_id }) => {
                if let Some(state) = self.pending(&question_id) {
                    state.status = QuestionStatus::Cancelled;
                }
            }
            _ => {}
        }
    }

    fn pending(&mut self, id: &str) -> Option<&mut QuestionState> {
        self.states
            .iter_mut()
            .find(|s| s.question.id == id && s.status == QuestionStatus::Pending)
    }

    /// The states now. A pending question past its deadline is expired.
    pub fn states(&self) -> Vec<QuestionState> {
        let now = Timestamp::now();
        let mut states = self.states.clone();
        for state in &mut states {
            if state.status == QuestionStatus::Pending
                && state.question.expires_at.is_some_and(|t| t <= now)
            {
                state.status = QuestionStatus::Expired;
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

#[cfg(not(wasm_browser))]
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
        if state.status != QuestionStatus::Pending {
            return Err(ChoiceError::new(
                state.status.as_str(),
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
            decision_title(&label),
            String::new(),
            "yappr".into(),
            "decision",
        )?;
        Ok((Some(entry), decision))
    })
}

/// The title of a decision entry: "Choice: " and the label. A long label is cut
/// at a character boundary, so the title fits the entry title limit. The full
/// label stays in the question.
#[cfg(not(wasm_browser))]
fn decision_title(label: &str) -> String {
    const PREFIX: &str = "Choice: ";
    const MORE: &str = "…";
    let room = crate::entry::MAX_TITLE - PREFIX.len();
    if label.len() <= room {
        return format!("{PREFIX}{label}");
    }
    let mut end = room - MORE.len();
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    format!("{PREFIX}{}{MORE}", &label[..end])
}

#[cfg(not(wasm_browser))]
pub fn cancel(store: &Store, id: &str) -> Result<()> {
    store.transaction(|entries| {
        let state = get(entries, id)?;
        if state.status == QuestionStatus::Cancelled {
            return Ok((None, ()));
        }
        if state.status != QuestionStatus::Pending {
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
        id: random_id(),
        title,
        body,
        source,
        options,
        responder,
        expires_at,
        approval,
        unlock: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(choices: serde_json::Value) -> Entry {
        serde_json::from_value(serde_json::json!({
            "v": 1, "id": 1, "time": "2026-11-02T10:00:00Z", "source": "ci",
            "kind": "question", "title": "Deploy?", "body": "", "choices": choices,
        }))
        .unwrap()
    }

    fn question(value: &str) -> serde_json::Value {
        serde_json::json!({"type": "question", "question": {
            "id": random_id(), "title": "Deploy?", "body": "", "source": "ci",
            "options": [{"value": value, "label": "Yes"}, {"value": "no", "label": "No"}],
            "responder": iroh::SecretKey::generate().public(),
            "expires_at": null, "approval": false,
        }})
    }

    #[test]
    fn a_record_that_breaks_its_rules_is_not_a_record() {
        assert!(super::question(&entry(question("yes"))).is_some());
        // A terminal escape in a value must never reach a reader.
        assert!(super::question(&entry(question("yes\u{1b}]0;x\u{7}"))).is_none());
        assert!(list(&[entry(question("yes\u{1b}[2J"))]).is_empty());

        let decision = |value: &str, id: String| {
            entry(serde_json::json!({"type": "decision", "decision": {
                "question_id": id, "submission_id": random_id(), "value": value,
                "responder": iroh::SecretKey::generate().public(),
                "time": "2026-11-02T10:00:00Z",
            }}))
        };
        assert!(closed(&decision("yes", random_id())).is_some());
        assert!(closed(&decision("yes\u{1b}[2J", random_id())).is_none());
        assert!(closed(&decision("yes", "not an id".into())).is_none());
        let cancelled =
            |id: &str| entry(serde_json::json!({"type": "cancelled", "question_id": id}));
        assert!(closed(&cancelled(&random_id())).is_some());
        assert!(closed(&cancelled("\u{1b}[2J")).is_none());
    }
}
