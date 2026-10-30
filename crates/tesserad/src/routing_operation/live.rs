use super::{Authority, RunResult, actor::Job, drive};
use crate::{engine_thread::EngineHandle, routing_receiver::OperationSocket};
use std::sync::Arc;
use tessera_control_types::{CommandError, Reply};

struct Dispatch {
    engine: EngineHandle,
    job: Arc<Job>,
}
impl Authority for Dispatch {
    async fn dispatch(&mut self) -> Result<(), CommandError> {
        self.engine.receiver_dispatch(self.job.clone()).await
    }
}

async fn run(engine: EngineHandle, job: Arc<Job>) -> RunResult {
    let deadline = job.deadline;
    match OperationSocket::connect(&job.target).await {
        Ok(mut socket) => {
            drive(
                &job.plan,
                &mut socket,
                &mut Dispatch {
                    engine: engine.clone(),
                    job: job.clone(),
                },
                deadline,
            )
            .await
        }
        Err(error) => RunResult::Refused(error),
    }
}

/// Called by the actor after admission, before attempting response delivery.
/// The supervisor outlives the request and observes panic as well as success.
pub(crate) fn launch(
    runtime: &tokio::runtime::Handle,
    engine: EngineHandle,
    job: Arc<Job>,
    reply: tokio::sync::oneshot::Sender<Result<Reply, CommandError>>,
) {
    let worker = run(engine.clone(), job.clone());
    super::worker::spawn(
        runtime,
        job.exited.clone(),
        worker,
        move |outcome| async move {
            let result = engine
                .receiver_finish(job, outcome.unwrap_or(RunResult::Stopped))
                .await;
            let _ = reply.send(result);
        },
    );
}
