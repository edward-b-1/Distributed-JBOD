use crate::{
    config::WorkerConfig,
    journal::{Journal, OpenMode},
    model::{
        operation_id, Consent, Description, DeviceInfo, Operation, Phase, Plan, Reply, Request,
        FORMAT,
    },
    storage::Root,
};
use anyhow::{ensure, Context, Result};
use djbod_core::{erasure::ShardIndex, record::DeviceId};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

const LEASE: Duration = Duration::from_secs(120);

#[derive(Clone, Serialize, Deserialize)]
struct Authorization {
    plan: Plan,
    controller: String,
    consent: Consent,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Entry {
    Initialized {
        id: Uuid,
        node: djbod_core::cluster::NodeId,
        cluster: Uuid,
    },
    Authorized {
        run: Uuid,
        authorization: Box<Authorization>,
    },
    Operation {
        value: Operation,
    },
}

struct Session {
    run: Uuid,
    controller: String,
    token: Uuid,
    until: Instant,
}

pub struct Worker {
    config: WorkerConfig,
    roots: BTreeMap<DeviceId, Root>,
    journal: Journal,
    journal_id: Uuid,
    authorizations: BTreeMap<Uuid, Authorization>,
    operations: BTreeMap<String, Operation>,
    session: Option<Session>,
}

impl Worker {
    pub fn open(config: WorkerConfig) -> Result<Self> {
        config.validate()?;
        let mut roots = BTreeMap::new();
        for path in &config.devices {
            for old in roots.values() {
                let old: &Root = old;
                ensure!(
                    !path.starts_with(&old.path) && !old.path.starts_with(path),
                    "overlapping device roots"
                );
            }
            let root = Root::open(path, config.cluster)?;
            ensure!(
                !config.journal.starts_with(path),
                "worker journal is inside a device root"
            );
            ensure!(roots.insert(root.id, root).is_none(), "duplicate device ID");
        }
        let mut journal = Journal::open(&config.journal, OpenMode::CreateOrOpen)?;
        if journal.entries.is_empty() {
            journal.append(&Entry::Initialized {
                id: Uuid::new_v4(),
                node: config.node,
                cluster: config.cluster,
            })?;
        }
        let entries = journal.read::<Entry>()?;
        let journal_id = match entries.first() {
            Some(Entry::Initialized { id, node, cluster })
                if *node == config.node && *cluster == config.cluster =>
            {
                *id
            }
            _ => anyhow::bail!("journal has no matching worker identity"),
        };
        let mut authorizations = BTreeMap::new();
        let mut operations = BTreeMap::new();
        for entry in entries {
            match entry {
                Entry::Initialized { .. } => {}
                Entry::Authorized { run, authorization } => {
                    authorization.plan.validate()?;
                    authorization.consent.validate(&authorization.plan)?;
                    ensure!(
                        authorization.plan.document.cluster_id == config.cluster,
                        "journal belongs to a different cluster"
                    );
                    authorizations.insert(run, *authorization);
                }
                Entry::Operation { value } => {
                    operations.insert(value.id.clone(), value);
                }
            }
        }
        for operation in operations.values() {
            let authorization = authorizations
                .get(&operation.run)
                .context("journal operation has no authorization")?;
            authorization
                .plan
                .validate_operation(config.node, operation)?;
        }
        Ok(Self {
            config,
            roots,
            journal,
            journal_id,
            authorizations,
            operations,
            session: None,
        })
    }

    pub fn description(&self) -> Description {
        Description {
            format: FORMAT,
            node: self.config.node,
            cluster: self.config.cluster,
            journal_id: self.journal_id,
            devices: self
                .roots
                .values()
                .map(|r| DeviceInfo {
                    device: r.id,
                    path: r.path.clone(),
                })
                .collect(),
        }
    }

    fn authorize(
        &mut self,
        controller: &str,
        plan: Plan,
        run: Uuid,
        consent: Consent,
    ) -> Result<Reply> {
        plan.validate()?;
        consent.validate(&plan)?;
        ensure!(
            plan.document.cluster_id == self.config.cluster,
            "wrong cluster"
        );
        let expected = plan
            .workers
            .iter()
            .find(|w| w.node == self.config.node)
            .context("worker absent from plan")?;
        ensure!(
            *expected == self.description(),
            "worker device identities/paths differ from plan"
        );
        if let Some(session) = &self.session {
            ensure!(
                Instant::now() >= session.until
                    || (session.run == run && session.controller == controller),
                "another controller/run holds the worker lease"
            );
        }
        ensure!(
            !self
                .operations
                .values()
                .any(|o| o.run != run && matches!(o.phase, Phase::Prepared | Phase::Intent)),
            "another run has unresolved operations; reconcile that run first"
        );
        if let Some(existing) = self.authorizations.get(&run) {
            ensure!(
                existing.plan.id == plan.id && existing.controller == controller,
                "run identity or controller changed"
            );
        } else {
            let authorization = Authorization {
                plan,
                controller: controller.to_owned(),
                consent,
            };
            self.journal.append(&Entry::Authorized {
                run,
                authorization: Box::new(authorization.clone()),
            })?;
            self.authorizations.insert(run, authorization);
        }
        // A new token fences delayed requests from the old coordinator session.
        let token = Uuid::new_v4();
        self.session = Some(Session {
            run,
            controller: controller.to_owned(),
            token,
            until: Instant::now() + LEASE,
        });
        Ok(Reply::Authorized { token })
    }

    fn session_plan(&mut self, controller: &str, run: Uuid, token: Uuid) -> Result<Plan> {
        let session = self
            .session
            .as_mut()
            .context("run is not authorized in this worker session")?;
        ensure!(
            session.run == run
                && session.controller == controller
                && session.token == token
                && Instant::now() < session.until,
            "expired or fenced worker session"
        );
        session.until = Instant::now() + LEASE;
        Ok(self
            .authorizations
            .get(&run)
            .context("missing authorization")?
            .plan
            .clone())
    }

    fn record_operation(&mut self, operation: Operation) -> Result<()> {
        self.journal.append(&Entry::Operation {
            value: operation.clone(),
        })?;
        self.operations.insert(operation.id.clone(), operation);
        Ok(())
    }

    fn event_operations(&self, run: Uuid, sequence: u64) -> Vec<Operation> {
        self.operations
            .values()
            .filter(|o| o.run == run && o.event.sequence == sequence)
            .cloned()
            .collect()
    }

    fn prepare(&mut self, plan: &Plan, run: Uuid, sequence: u64) -> Result<Reply> {
        let event = plan.event(sequence)?;
        let indices = plan.local_indices(&event, self.config.node)?;
        let record = &plan.objects[event.object].record;
        let mut prepared = Vec::new();
        for index in indices {
            let id = operation_id(run, sequence, index);
            if let Some(existing) = self.operations.get(&id) {
                ensure!(existing.phase != Phase::Cancelled, "event was cancelled");
                continue;
            }
            let device = record
                .device_for(ShardIndex(index))
                .context("missing device")?;
            let root = self
                .roots
                .get(&device)
                .context("device is not allowed on this worker")?;
            let shard = root.shard(record, index, false)?;
            let (byte, mask) = plan.position(&event, index)?;
            let mutation = shard.prepare(event.stripe, byte, mask, device)?;
            prepared.push(Operation {
                id,
                run,
                event: event.clone(),
                mutation,
                phase: Phase::Prepared,
            });
        }
        for operation in prepared {
            self.record_operation(operation)?;
        }
        Ok(Reply::Operations(self.event_operations(run, sequence)))
    }

    fn apply(&mut self, plan: &Plan, run: Uuid, sequence: u64) -> Result<Reply> {
        let event = plan.event(sequence)?;
        let record = &plan.objects[event.object].record;
        let indices = plan.local_indices(&event, self.config.node)?;
        for index in &indices {
            let operation = self
                .operations
                .get(&operation_id(run, sequence, *index))
                .context("event was not prepared")?;
            ensure!(operation.phase != Phase::Cancelled, "event was cancelled");
        }
        for index in indices {
            let id = operation_id(run, sequence, index);
            let mut operation = self
                .operations
                .get(&id)
                .context("missing prepared operation")?
                .clone();
            if operation.phase == Phase::Applied {
                continue;
            }
            let prior_intent = operation.phase == Phase::Intent;
            if !prior_intent {
                operation.phase = Phase::Intent;
                self.record_operation(operation.clone())?;
            }
            let root = self
                .roots
                .get(&operation.mutation.device)
                .context("device no longer allowed")?;
            let shard = root.shard(record, index, true)?;
            shard.apply(event.stripe, &operation.mutation, prior_intent)?;
            // Check the pathname still refers to the file that was mutated.
            let current = root.shard(record, index, false)?.probe(event.stripe)?;
            ensure!(
                current
                    .fingerprint
                    .same_file(&operation.mutation.fingerprint)
                    && current.actual_checksum == operation.mutation.after_checksum,
                "uncertain: pathname or block changed during mutation"
            );
            operation.phase = Phase::Applied;
            self.record_operation(operation)?;
        }
        Ok(Reply::Operations(self.event_operations(run, sequence)))
    }

    fn cancel(&mut self, run: Uuid, sequence: u64) -> Result<Reply> {
        let operations = self.event_operations(run, sequence);
        ensure!(
            operations
                .iter()
                .all(|o| matches!(o.phase, Phase::Prepared | Phase::Cancelled)),
            "apply began; cancellation cannot undo mutations"
        );
        for mut operation in operations {
            if operation.phase != Phase::Cancelled {
                operation.phase = Phase::Cancelled;
                self.record_operation(operation)?;
            }
        }
        Ok(Reply::Operations(self.event_operations(run, sequence)))
    }

    pub fn handle(&mut self, certificate: Option<&str>, request: Request) -> Result<Reply> {
        self.journal.check_writable()?;
        ensure!(
            self.config.transport != djbod_core::cluster::Transport::Tls || certificate.is_some(),
            "TlsRequired: worker transport tls requires a TLS connection with a client certificate"
        );
        ensure!(
            self.config.allowed_controllers.is_empty()
                || certificate.is_some_and(|controller| self
                    .config
                    .allowed_controllers
                    .iter()
                    .any(|f| f.eq_ignore_ascii_case(controller))),
            "controller certificate is not allowed"
        );
        // Plain and anonymous TLS clients have no authenticated identity.
        // They share this label; run IDs and session tokens still fence retries.
        let controller = certificate.unwrap_or("anonymous");
        match request {
            Request::Describe => Ok(Reply::Description(self.description())),
            Request::Probe {
                record,
                index,
                stripe,
            } => {
                let device = record
                    .device_for(ShardIndex(index))
                    .context("invalid shard index")?;
                let root = self.roots.get(&device).context("device is not allowed")?;
                Ok(Reply::Probe(
                    root.shard(&record, index, false)?.probe(stripe)?,
                ))
            }
            Request::Authorize { plan, run, consent } => {
                self.authorize(controller, *plan, run, consent)
            }
            Request::Prepare {
                run,
                token,
                sequence,
            } => {
                let plan = self.session_plan(controller, run, token)?;
                self.prepare(&plan, run, sequence)
            }
            Request::Apply {
                run,
                token,
                sequence,
            } => {
                let plan = self.session_plan(controller, run, token)?;
                self.apply(&plan, run, sequence)
            }
            Request::Status {
                run,
                token,
                sequence,
            } => {
                self.session_plan(controller, run, token)?;
                Ok(Reply::Operations(self.event_operations(run, sequence)))
            }
            Request::Cancel {
                run,
                token,
                sequence,
            } => {
                self.session_plan(controller, run, token)?;
                self.cancel(run, sequence)
            }
            Request::Finish { run, token } => {
                self.session_plan(controller, run, token)?;
                ensure!(!self.operations.values().any(|o| o.run == run && matches!(o.phase, Phase::Prepared | Phase::Intent)),
                    "run has unresolved operations");
                self.session = None;
                Ok(Reply::Finished)
            }
        }
    }
}
