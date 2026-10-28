//! Serializable Promise state and FIFO reaction queue. The interpreter does
//! not yet expose these as JavaScript Promise objects or dispatch jobs.
use super::heap::VmValue;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct PromiseId(u64);

/// An interpreter-owned continuation. The scheduler stores an opaque ID;
/// future execution frames will own the callback and its lexical environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ContinuationId(pub(super) u64);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) enum Outcome {
    Fulfilled(VmValue),
    Rejected(VmValue),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Promise {
    outcome: Option<Outcome>,
    reactions: Vec<ContinuationId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Job {
    pub continuation: ContinuationId,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct PromiseScheduler {
    next_id: u64,
    promises: BTreeMap<PromiseId, Promise>,
    jobs: VecDeque<Job>,
}

// These primitives will be used by the async interpreter in the next step.
#[allow(dead_code)]
impl PromiseScheduler {
    pub(super) fn roots(&self) -> Vec<VmValue> {
        self.promises
            .values()
            .filter_map(|promise| promise.outcome.as_ref())
            .chain(self.jobs.iter().map(|job| &job.outcome))
            .map(|outcome| match outcome {
                Outcome::Fulfilled(value) | Outcome::Rejected(value) => value.clone(),
            })
            .collect()
    }
    pub(super) fn create(&mut self) -> Result<PromiseId> {
        let id = PromiseId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("promise id exhausted")?;
        self.promises.insert(
            id,
            Promise {
                outcome: None,
                reactions: Vec::new(),
            },
        );
        Ok(id)
    }

    pub(super) fn subscribe(&mut self, id: PromiseId, continuation: ContinuationId) -> Result<()> {
        let promise = self.promises.get_mut(&id).context("unknown promise")?;
        match &promise.outcome {
            Some(outcome) => self.jobs.push_back(Job {
                continuation,
                outcome: outcome.clone(),
            }),
            None => promise.reactions.push(continuation),
        }
        Ok(())
    }

    /// Return false if the promise was already settled; never queue its
    /// reactions twice, including after recovery and duplicate host receipts.
    pub(super) fn settle(&mut self, id: PromiseId, outcome: Outcome) -> Result<bool> {
        let promise = self.promises.get_mut(&id).context("unknown promise")?;
        if promise.outcome.is_some() {
            return Ok(false);
        }
        ensure!(
            promise.outcome.replace(outcome.clone()).is_none(),
            "promise already settled"
        );
        for continuation in std::mem::take(&mut promise.reactions) {
            self.jobs.push_back(Job {
                continuation,
                outcome: outcome.clone(),
            });
        }
        Ok(true)
    }

    pub(super) fn pop_job(&mut self) -> Option<Job> {
        self.jobs.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::super::heap::{Heap, HeapNode, JsNumber};
    use super::*;

    #[test]
    fn reactions_are_fifo_and_survive_snapshot_without_duplicate_settlement() {
        let mut scheduler = PromiseScheduler::default();
        let first = scheduler.create().unwrap();
        let second = scheduler.create().unwrap();
        scheduler.subscribe(first, ContinuationId(1)).unwrap();
        scheduler.subscribe(first, ContinuationId(2)).unwrap();
        scheduler.subscribe(second, ContinuationId(3)).unwrap();
        scheduler
            .settle(second, Outcome::Rejected(VmValue::String("failure".into())))
            .unwrap();
        scheduler
            .settle(
                first,
                Outcome::Fulfilled(VmValue::Number(JsNumber::new(42.0))),
            )
            .unwrap();
        let snapshot = serde_json::to_vec(&scheduler).unwrap();
        let mut restored: PromiseScheduler = serde_json::from_slice(&snapshot).unwrap();
        assert!(
            !restored
                .settle(
                    first,
                    Outcome::Fulfilled(VmValue::Number(JsNumber::new(99.0)))
                )
                .unwrap()
        );
        restored.subscribe(first, ContinuationId(4)).unwrap();
        for (id, outcome) in [
            (3, Outcome::Rejected(VmValue::String("failure".into()))),
            (1, Outcome::Fulfilled(VmValue::Number(JsNumber::new(42.0)))),
            (2, Outcome::Fulfilled(VmValue::Number(JsNumber::new(42.0)))),
            (4, Outcome::Fulfilled(VmValue::Number(JsNumber::new(42.0)))),
        ] {
            assert_eq!(
                restored.pop_job(),
                Some(Job {
                    continuation: ContinuationId(id),
                    outcome
                })
            );
        }
        assert_eq!(restored.pop_job(), None);
    }

    #[test]
    fn queued_promise_result_keeps_heap_identity_across_snapshot() {
        let mut heap = Heap::default();
        let object = heap.allocate(HeapNode::Object(Vec::new())).unwrap();
        let mut scheduler = PromiseScheduler::default();
        let id = scheduler.create().unwrap();
        scheduler.subscribe(id, ContinuationId(1)).unwrap();
        scheduler
            .settle(id, Outcome::Fulfilled(VmValue::Reference(object)))
            .unwrap();
        let snapshot = serde_json::to_vec(&(heap, scheduler)).unwrap();
        let (mut heap, mut scheduler): (Heap, PromiseScheduler) =
            serde_json::from_slice(&snapshot).unwrap();
        heap.set_property(object, "state".into(), VmValue::String("updated".into()))
            .unwrap();
        let job = scheduler.pop_job().unwrap();
        assert_eq!(job.outcome, Outcome::Fulfilled(VmValue::Reference(object)));
        assert_eq!(
            heap.property(object, "state").unwrap(),
            VmValue::String("updated".into())
        );
    }

    #[test]
    fn pending_promise_and_subscriptions_survive_snapshot() {
        let mut scheduler = PromiseScheduler::default();
        let id = scheduler.create().unwrap();
        scheduler.subscribe(id, ContinuationId(7)).unwrap();
        let snapshot = serde_json::to_vec(&scheduler).unwrap();
        let mut restored: PromiseScheduler = serde_json::from_slice(&snapshot).unwrap();
        restored
            .settle(id, Outcome::Fulfilled(VmValue::String("ready".into())))
            .unwrap();
        assert_eq!(
            restored.pop_job(),
            Some(Job {
                continuation: ContinuationId(7),
                outcome: Outcome::Fulfilled(VmValue::String("ready".into()))
            })
        );
        assert!(restored.create().unwrap() != id);
    }
}
