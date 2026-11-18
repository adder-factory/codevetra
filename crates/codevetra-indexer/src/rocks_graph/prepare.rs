use codevetra_db::{CanonicalGenerationFacts, rocks::GraphPreparation};

use super::{
    GraphPublicationInput, NativePipelineError, NativeRocksGraphError, PipelineStage, RocksError,
    StageRunner,
};
use crate::{
    NativePipelineDeadlines, StageCapacity, StageEnvelope, StageExecution, StageFold,
    StageItemBudget, StageItemFailure, StageItemMeta, StageOutput, StageRunConfig, StageSequence,
    StageWorkItem, StageWorkload,
};

#[cfg(test)]
mod tests;

/// One admitted publication work item carries its payload and limits together.
pub(super) struct Preparation {
    pub(super) facts: CanonicalGenerationFacts,
    pub(super) graph: GraphPreparation,
    pub(super) policy: Policy,
}

pub(super) struct Policy {
    pub(super) maximum_bytes: u64,
    pub(super) deadlines: NativePipelineDeadlines,
}

impl Preparation {
    pub(super) fn run(
        self,
        runner: &StageRunner,
    ) -> impl std::future::Future<Output = Result<GraphPublicationInput, NativeRocksGraphError>>
    {
        // Keep the owned canonical payload out of the enclosing coordinator's
        // async state. The preparation and its reservation still live until completion.
        Box::pin(self.run_with(runner, GraphPreparation::prepare))
    }

    async fn run_with<Work>(
        self,
        runner: &StageRunner,
        work: Work,
    ) -> Result<GraphPublicationInput, NativeRocksGraphError>
    where
        Work: Fn(
                GraphPreparation,
                CanonicalGenerationFacts,
                u64,
            ) -> Result<GraphPublicationInput, RocksError>
            + Send
            + Sync
            + 'static,
    {
        let (deadlines, item_deadline) = self.policy.deadlines.publication_policy();
        let work = std::sync::Arc::new(work);
        let inputs = [StageEnvelope::new(
            StageItemMeta::new(
                StageSequence::new(0),
                0_u8,
                StageItemBudget::new(self.policy.maximum_bytes, 0, item_deadline),
            ),
            self,
        )];
        let execution = StageExecution::new(
            StageRunConfig::new(PipelineStage::Copy, StageCapacity::new(1, 0), deadlines),
            StageWorkload::new(inputs, move |item: StageWorkItem<u8, Preparation>| {
                let work = std::sync::Arc::clone(&work);
                async move {
                    let cancellation = item.cancellation();
                    let (
                        _,
                        _,
                        Preparation {
                            facts,
                            graph,
                            policy,
                        },
                    ) = item.into_parts();
                    let result = tokio::task::block_in_place(|| {
                        let request = cancellation.storage_request(graph.request())?;
                        work(graph.with_request(&request), facts, policy.maximum_bytes)
                    });
                    Ok::<_, StageItemFailure>(result)
                }
            }),
            StageFold::new(
                None,
                |output: &mut Option<Result<GraphPublicationInput, RocksError>>,
                 item: StageOutput<u8, Result<GraphPublicationInput, RocksError>>| {
                    *output = Some(item.into_parts().1);
                    Ok(())
                },
            ),
        );
        runner
            .execute(execution)
            .await
            .map_err(NativePipelineError::from)?
            .ok_or(NativePipelineError::Incomplete {
                stage: PipelineStage::Copy,
            })?
            .map_err(NativeRocksGraphError::from)
    }
}
