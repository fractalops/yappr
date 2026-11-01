//! The entries of a feed that still need attention: questions with no final
//! decision, and subjects whose newest state is "warn" or "alert".
use std::collections::{BTreeMap, HashMap};

use iroh::EndpointId;
use jiff::Timestamp;

use crate::{choices, entry::Entry};

/// The entry kinds that can open or close an entry.
pub const KINDS: [&str; 4] = ["question", "decision", "cancelled", "state"];

/// Open entries, built one entry at a time in feed order.
#[derive(Debug, Default)]
pub struct Open {
    /// Keep only the questions that this device can answer. `None` keeps all.
    responder: Option<EndpointId>,
    entries: BTreeMap<u64, Entry>,
    /// Question ID to entry ID.
    questions: HashMap<String, u64>,
    /// Subject to the entry ID of its newest state, if that state is open.
    states: HashMap<String, u64>,
}

impl Open {
    pub fn new(responder: Option<EndpointId>) -> Self {
        Self {
            responder,
            ..Default::default()
        }
    }

    /// Add the next entry. Returns true if this entry is open now.
    pub fn push(&mut self, entry: &Entry) -> bool {
        if let Some(question) = choices::question(entry) {
            if self.responder.is_some_and(|me| me != question.responder)
                || expired(question.expires_at)
            {
                return false;
            }
            self.questions.insert(question.id, entry.id);
            self.entries.insert(entry.id, entry.clone());
            return true;
        }
        if let Some(id) = choices::closed(entry) {
            if let Some(at) = self.questions.remove(&id) {
                self.entries.remove(&at);
            }
            return false;
        }
        let Some(subject) = entry.subject.as_ref().filter(|_| entry.kind == "state") else {
            return false;
        };
        if let Some(at) = self.states.remove(subject) {
            self.entries.remove(&at);
        }
        if entry.level_rank() == 0 {
            return false;
        }
        self.states.insert(subject.clone(), entry.id);
        self.entries.insert(entry.id, entry.clone());
        true
    }

    /// The open entries in feed order. A question past its deadline is not
    /// open.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .values()
            .filter(|e| choices::question(e).is_none_or(|q| !expired(q.expires_at)))
    }
}

fn expired(deadline: Option<Timestamp>) -> bool {
    deadline.is_some_and(|t| t <= Timestamp::now())
}

#[cfg(all(test, not(wasm_browser)))]
mod tests {
    use super::*;
    use crate::{
        choices::{self, Submission},
        entry::Draft,
        store::Store,
    };

    fn state(store: &Store, subject: &str, level: &str) -> Entry {
        store
            .append(Draft {
                source: "nas".into(),
                kind: "state".into(),
                level: Some(level.into()),
                subject: Some(subject.into()),
                title: Some(format!("{subject} is {level}")),
                body: String::new(),
                format: None,
                data: None,
                extra: Default::default(),
            })
            .unwrap()
    }

    fn ask(store: &Store, responder: EndpointId) -> choices::Question {
        let question = choices::new_question(
            "Deploy?".into(),
            String::new(),
            "ci".into(),
            choices::approval_options(),
            responder,
            None,
            true,
        );
        choices::create(store, question).unwrap()
    }

    fn open_ids(store: &Store, responder: Option<EndpointId>) -> Vec<u64> {
        let mut open = Open::new(responder);
        for entry in store.entries().unwrap() {
            open.push(&entry);
        }
        open.entries().map(|e| e.id).collect()
    }

    #[test]
    fn a_state_is_open_until_a_newer_state_of_its_subject_is_info() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let late = state(&store, "backup/nas", "warn");
        let disk = state(&store, "disk/root", "alert");
        state(&store, "dns", "info");
        assert_eq!(open_ids(&store, None), [late.id, disk.id]);

        state(&store, "backup/nas", "info");
        assert_eq!(open_ids(&store, None), [disk.id]);
    }

    #[test]
    fn a_question_is_open_for_its_responder_until_it_is_answered_or_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let me = iroh::SecretKey::generate().public();
        let other = iroh::SecretKey::generate().public();
        let mine = ask(&store, me);
        let theirs = ask(&store, other);
        assert_eq!(open_ids(&store, Some(me)), [1]);
        assert_eq!(open_ids(&store, None).len(), 2);

        store.allow_list().allow(me).unwrap();
        choices::decide(
            &store,
            me,
            Submission {
                id: choices::random_id(),
                question_id: mine.id.clone(),
                value: "approve".into(),
            },
        )
        .unwrap();
        choices::cancel(&store, &theirs.id).unwrap();
        assert!(open_ids(&store, None).is_empty());
    }

    #[test]
    fn a_question_past_its_deadline_is_not_open() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let me = iroh::SecretKey::generate().public();
        let mut question = choices::new_question(
            "Deploy?".into(),
            String::new(),
            "ci".into(),
            choices::approval_options(),
            me,
            Some(Timestamp::now() + jiff::SignedDuration::from_millis(50)),
            true,
        );
        question = choices::create(&store, question).unwrap();
        assert_eq!(open_ids(&store, Some(me)).len(), 1);
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(open_ids(&store, Some(me)).is_empty());
        assert_eq!(
            choices::id_of_entry(&store.entries().unwrap(), 1).unwrap(),
            question.id
        );
    }
}
