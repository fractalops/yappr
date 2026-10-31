//! Persistent submissions that can resume after a connection fails.
use std::path::Path;

use anyhow::{bail, Context, Result};
use iroh::Endpoint;
use serde::{Deserialize, Serialize};

use crate::{
    choices::{ChoiceError, Decision, Submission},
    control, disk, EndpointTicket,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delivery {
    pub writer: String,
    pub ticket: String,
    pub submission: Submission,
    pub status: String,
    pub decision: Option<Decision>,
    pub error: Option<String>,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct Outbox {
    deliveries: Vec<Delivery>,
}

pub fn list(dir: &Path) -> Result<Vec<Delivery>> {
    let outbox: Outbox = disk::read(&dir.join("outbox.json"))?;
    Ok(outbox.deliveries)
}

pub fn queue(
    dir: &Path,
    ticket: &EndpointTicket,
    question_id: String,
    value: String,
) -> Result<Delivery> {
    if !crate::choices::valid_id(&question_id) {
        bail!("the question ID is not valid");
    }
    let _lock = disk::lock(dir, "outbox.lock")?;
    let path = dir.join("outbox.json");
    let mut outbox: Outbox = disk::read(&path)?;
    let writer = ticket.endpoint_addr().id.to_string();
    if let Some(delivery) = outbox.deliveries.iter().find(|d| {
        d.writer == writer && d.submission.question_id == question_id && d.status != "rejected"
    }) {
        if delivery.submission.value != value {
            bail!("this device already selected a different option for this question");
        }
        return Ok(delivery.clone());
    }
    let delivery = Delivery {
        writer,
        ticket: ticket.to_string(),
        submission: Submission {
            id: disk::random_id(),
            question_id,
            value,
        },
        status: "queued".into(),
        decision: None,
        error: None,
    };
    outbox.deliveries.push(delivery.clone());
    disk::write(&path, &outbox)?;
    Ok(delivery)
}

pub async fn deliver(dir: &Path, endpoint: &Endpoint, delivery: &Delivery) -> Result<Decision> {
    deliver_with(
        dir,
        &control::ControlClient::new(endpoint.clone()),
        delivery,
    )
    .await
}

pub async fn deliver_with(
    dir: &Path,
    client: &control::ControlClient,
    delivery: &Delivery,
) -> Result<Decision> {
    if let Some(decision) = &delivery.decision {
        return Ok(decision.clone());
    }
    let ticket: EndpointTicket = delivery.ticket.parse()?;
    if ticket.endpoint_addr().id.to_string() != delivery.writer {
        bail!("the delivery ticket belongs to another writer");
    }
    let result = client
        .submit(ticket.endpoint_addr().clone(), delivery.submission.clone())
        .await;
    let _lock = disk::lock(dir, "outbox.lock")?;
    let path = dir.join("outbox.json");
    let mut outbox: Outbox = disk::read(&path)?;
    let saved = outbox
        .deliveries
        .iter_mut()
        .find(|d| d.submission.id == delivery.submission.id)
        .context("the pending submission does not exist")?;
    match result {
        Ok(decision) => {
            if decision.question_id != delivery.submission.question_id
                || decision.submission_id != delivery.submission.id
                || decision.value != delivery.submission.value
                || decision.responder != client.endpoint().id()
            {
                bail!("the writer returned a decision for a different submission");
            }
            saved.status = "delivered".into();
            saved.decision = Some(decision.clone());
            saved.error = None;
            disk::write(&path, &outbox)?;
            Ok(decision)
        }
        Err(error) => {
            if error
                .downcast_ref::<ChoiceError>()
                .is_some_and(|e| e.code != "storage")
            {
                saved.status = "rejected".into();
            }
            saved.error = Some(format!("{error:#}"));
            disk::write(&path, &outbox)?;
            Err(error)
        }
    }
}
