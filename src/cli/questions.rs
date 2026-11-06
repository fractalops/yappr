//! Question delivery, decisions, and cancellation.

use std::time::Duration;

use anyhow::{bail, Result};
use jiff::Timestamp;
use tokio::time::timeout;
use yappr::{
    choices::{self, Choice, Question, QuestionStatus},
    client::{Event, Follower, Unreachable},
    control::ControlClient,
    deliver::{self, Hello},
    net, outbox, peers,
    proto::{Request, Stored},
    server::Feed,
    store::Store,
    EndpointTicket,
};

use super::{
    args::{CommonArgs, QuestionArgs},
    context::hostname,
    shown,
    writer::{deliver_now, own_receivers},
};

/// One question for people: status, title, the choices or the answer, then
/// the full ID for `wait`, `cancel`, and `answer`.
pub(super) fn question_line(state: &choices::QuestionState) -> String {
    let detail = match &state.decision {
        Some(decision) => format!("answer: {}", decision.value),
        None => {
            let values: Vec<&str> = state
                .question
                .options
                .iter()
                .map(|o| o.value.as_str())
                .collect();
            format!("choices: {}", values.join(", "))
        }
    };
    format!(
        "{:<9} {}  ({detail})  {}",
        state.status, state.question.title, state.question.id
    )
}

#[derive(Debug)]
pub(super) struct QuestionOutcome {
    pub(super) code: i32,
    message: &'static str,
}

impl std::fmt::Display for QuestionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for QuestionOutcome {}

pub(super) async fn ask(
    common: &CommonArgs,
    mut args: QuestionArgs,
    options: Vec<Choice>,
    approval: bool,
) -> Result<()> {
    let (to, title) = args.target()?;
    let store = common.store()?;
    let responder = peers::resolve(store.dir(), &to)?.id;
    if !store.allow_list().contains(&responder) {
        bail!("allow the responder first: yappr allow {to}");
    }
    let expires_at = Timestamp::now().checked_add(jiff::SignedDuration::try_from(args.timeout)?)?;
    let question = choices::new_question(
        title,
        args.body,
        args.source.unwrap_or_else(hostname),
        options,
        responder,
        Some(expires_at),
        approval,
    );
    let question: Question = choices::create_with(&store, question, args.details.into_details())?;
    if args.no_wait {
        println!("{}", question.id);
        return deliver_now(common, &store, store.last_id()?).await;
    }
    eprintln!(
        "Question {}. Waiting for {}.",
        question.id, question.responder
    );
    tokio::select! {
        result = wait(common, &question.id) => result,
        _ = tokio::signal::ctrl_c() => {
            // A decision can reach disk before the cancellation. It remains
            // final, so report it in place of the cancellation.
            let cancelled = choices::cancel(&store, &question.id);
            let state = choices::get(&store.entries()?, &question.id)?;
            match state.status {
                QuestionStatus::Pending => cancelled?,
                QuestionStatus::Cancelled => {}
                _ => if let Some(result) = finish(state) { return result; },
            }
            Err(QuestionOutcome { code: 8, message: "the wait stopped. Use yappr wait to read the final state" }.into())
        }
    }
}

pub(super) async fn wait(common: &CommonArgs, id: &str) -> Result<()> {
    let state = final_state(common, id).await?;
    finish(state).expect("the question is in a final state")
}

/// Wait until the question `id` is answered, expired, or cancelled, and
/// deliver it to its device in the meantime. Returns its final state.
pub(super) async fn final_state(common: &CommonArgs, id: &str) -> Result<choices::QuestionState> {
    let store = common.store()?;
    // Read only the entries that arrive after each check.
    let mut reader = store.reader()?;
    let mut tracker = choices::Tracker::one(id);
    let mut delivery = None;
    loop {
        while let Some(entry) = reader.next_entry()? {
            tracker.push(&entry);
        }
        let state = tracker
            .states()
            .pop()
            .ok_or_else(|| choices::ChoiceError::new("not_found", "the question does not exist"))?;
        let pending = state.status == QuestionStatus::Pending;
        if pending && delivery.is_none() {
            delivery = Some(
                QuestionDelivery::start(common, &store, state.question.responder)
                    .await
                    .unwrap_or_else(|error| {
                        eprintln!("cannot deliver the question: {}", shown(&error));
                        None
                    }),
            );
        }
        if !pending {
            if let Some(delivery) = delivery.flatten() {
                delivery.finish(store.last_id()?).await;
            }
            return Ok(state);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Delivers the feed to the device that answers a question, while the
/// question waits and no running writer delivers it.
struct QuestionDelivery {
    endpoint: iroh::Endpoint,
    task: n0_future::task::AbortOnDropHandle<()>,
    progress: tokio::sync::watch::Receiver<Stored>,
}

impl QuestionDelivery {
    async fn start(
        common: &CommonArgs,
        store: &Store,
        device: iroh::EndpointId,
    ) -> Result<Option<Self>> {
        let Some((_, addr)) = own_receivers(store)?
            .into_iter()
            .find(|(_, addr)| addr.id == device)
        else {
            return Ok(None);
        };
        let secret = common.secret()?;
        let endpoint =
            net::bind(iroh::SecretKey::generate(), common.net(None)?, vec![], None).await?;
        let feed = Feed::open(store.clone()).await?;
        let (progress, seen) = tokio::sync::watch::channel(Stored::default());
        let transport = endpoint.id();
        let task = tokio::spawn(deliver::deliver_until_stopped(
            endpoint.clone(),
            feed,
            addr,
            move || Hello::granted(&secret, transport),
            progress,
        ));
        Ok(Some(Self {
            endpoint,
            task: n0_future::task::AbortOnDropHandle::new(task),
            progress: seen,
        }))
    }

    /// Give the device a few seconds to save the entries up to `through`,
    /// such as the decision, then stop.
    async fn finish(mut self, through: u64) {
        timeout(
            Duration::from_secs(5),
            self.progress.wait_for(|stored| stored.through >= through),
        )
        .await
        .ok();
        drop(self.task);
        self.endpoint.close().await;
    }
}

/// The result of a question in a final state, or `None` while it is pending.
fn finish(state: choices::QuestionState) -> Option<Result<()>> {
    let outcome = |code, message| Some(Err(QuestionOutcome { code, message }.into()));
    match state.status {
        QuestionStatus::Answered => {
            let Some(decision) = state.decision else {
                return Some(Err(anyhow::anyhow!("the decision is missing")));
            };
            println!("{}", decision.value);
            if state.question.approval && decision.value == "reject" {
                outcome(7, "the responder rejected the request")
            } else {
                Some(Ok(()))
            }
        }
        QuestionStatus::Expired => outcome(6, "the question expired"),
        QuestionStatus::Cancelled => outcome(8, "the question was cancelled"),
        QuestionStatus::Pending => None,
    }
}

/// The question ID of a local question, given as an entry ID or a question ID.
pub(super) fn local_question(common: &CommonArgs, id: String) -> Result<String> {
    match id.parse() {
        Ok(entry) => choices::id_of_entry(&common.store()?.entries()?, entry),
        Err(_) => Ok(id),
    }
}

/// The question ID of a writer's question, given as an entry ID or a question
/// ID. An entry ID needs the writer's question entry.
pub(super) async fn remote_question(
    common: &CommonArgs,
    ticket: &EndpointTicket,
    id: String,
) -> Result<String> {
    let Ok(entry_id) = id.parse::<u64>() else {
        return Ok(id);
    };
    let req = Request {
        since: entry_id.saturating_sub(1),
        limit: Some(1),
        kind_patterns: vec!["question".into()],
        ..Default::default()
    };
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let mut follower = Follower::new(endpoint.clone(), ticket.endpoint_addr().clone(), req);
    let result = async {
        while let Some(event) = follower.next().await? {
            if let Event::Entry(entry) = event {
                if entry.id == entry_id {
                    if let Some(question) = choices::question(&entry) {
                        return Ok(question.id);
                    }
                }
                break;
            }
            if let Event::CaughtUp { .. } = event {
                break;
            }
        }
        Err(
            choices::ChoiceError::new("not_found", &format!("entry #{entry_id} is not a question"))
                .into(),
        )
    }
    .await;
    endpoint.close().await;
    result
}

pub(super) async fn answer(
    common: &CommonArgs,
    ticket: EndpointTicket,
    id: String,
    value: String,
) -> Result<()> {
    let dir = common.dir()?;
    let delivery = outbox::queue(&dir, &ticket, id, value)?;
    let endpoint = net::bind(common.secret()?, common.net(None)?, vec![], None).await?;
    let client = ControlClient::new(endpoint.clone());
    let result = outbox::deliver_with(&dir, &client, &delivery).await;
    client.close().await;
    endpoint.close().await;
    match result {
        Ok(decision) => {
            println!("{}", serde_json::to_string(&decision)?);
            Ok(())
        }
        Err(error) if error.downcast_ref::<choices::ChoiceError>().is_some() => Err(error),
        Err(error) => {
            eprintln!(
                "The choice remains queued. Run yappr retry. {}",
                shown(&error)
            );
            Err(Unreachable {
                writer: ticket.endpoint_addr().id,
                reason: "the writer did not confirm the choice".into(),
            }
            .into())
        }
    }
}
