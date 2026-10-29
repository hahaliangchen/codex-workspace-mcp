//! Serializable Promise state and the FIFO reaction queue used by async frames.
//! General JavaScript Promise methods are introduced separately.
use super::heap::{Heap, HeapId, HeapNode, VmValue};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct PromiseId(u64);

/// An interpreter-owned continuation. The scheduler stores an opaque ID;
/// runtime frames own the saved state and lexical environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

// The legacy Promise primitives remain available for older snapshots.
#[allow(dead_code)]
impl PromiseScheduler {
    pub(super) fn create_heap_promise(&mut self, heap: &mut Heap) -> Result<HeapId> {
        heap.allocate(HeapNode::Promise { outcome: None, reactions: Vec::new(), handled: false })
    }
    pub(super) fn subscribe_heap_promise(&mut self, heap: &mut Heap, id: HeapId, continuation: ContinuationId) -> Result<()> {
        let HeapNode::Promise { outcome, reactions, handled } = heap.get_mut(id)? else { anyhow::bail!("not a promise") };
        *handled = true;
        match outcome {
            Some(outcome) => self.jobs.push_back(Job { continuation, outcome: outcome.clone() }),
            None => reactions.push(continuation),
        }
        Ok(())
    }
    pub(super) fn settle_heap_promise(&mut self, heap: &mut Heap, id: HeapId, result: Outcome) -> Result<bool> {
        let HeapNode::Promise { outcome, reactions, .. } = heap.get_mut(id)? else { anyhow::bail!("not a promise") };
        if outcome.is_some() { return Ok(false); }
        *outcome = Some(result.clone());
        for continuation in std::mem::take(reactions) {
            self.jobs.push_back(Job { continuation, outcome: result.clone() });
        }
        Ok(true)
    }
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

    pub(super) fn enqueue(&mut self, continuation: ContinuationId, outcome: Outcome) {
        self.jobs.push_back(Job { continuation, outcome });
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
    fn heap_promise_preserves_identity_results_and_fifo_jobs_across_restore() {
        let mut heap = Heap::default();
        let mut scheduler = PromiseScheduler::default();
        let object = heap.allocate(HeapNode::Object(Vec::new())).unwrap();
        let promise = scheduler.create_heap_promise(&mut heap).unwrap();
        scheduler.subscribe_heap_promise(&mut heap, promise, ContinuationId(1)).unwrap();
        scheduler.subscribe_heap_promise(&mut heap, promise, ContinuationId(2)).unwrap();
        scheduler.settle_heap_promise(&mut heap, promise, Outcome::Fulfilled(VmValue::Reference(object))).unwrap();
        let mut roots = scheduler.roots();
        roots.push(VmValue::Reference(promise));
        heap.collect(&roots).unwrap();
        let snapshot = serde_json::to_vec(&(heap, scheduler)).unwrap();
        let (mut heap, mut scheduler): (Heap, PromiseScheduler) = serde_json::from_slice(&snapshot).unwrap();
        assert!(!scheduler.settle_heap_promise(&mut heap, promise, Outcome::Fulfilled(VmValue::Null)).unwrap());
        scheduler.subscribe_heap_promise(&mut heap, promise, ContinuationId(3)).unwrap();
        for id in 1..=3 {
            assert_eq!(scheduler.pop_job(), Some(Job { continuation: ContinuationId(id), outcome: Outcome::Fulfilled(VmValue::Reference(object)) }));
        }
        assert_eq!(heap.get(promise).unwrap(), &HeapNode::Promise { outcome: Some(Outcome::Fulfilled(VmValue::Reference(object))), reactions: vec![], handled: true });
        assert!(heap.to_json(&VmValue::Reference(promise)).is_err());
        assert_eq!(super::super::js_methods::builtin(&mut heap, "JSON.stringify", &[VmValue::Reference(promise)]).unwrap(), VmValue::String("{}".into()));
        assert!(super::super::vm_eval::member(&heap, &VmValue::Reference(promise), &VmValue::String("content".into())).unwrap_err().to_string().contains("await"));
    }

    #[test]
    fn settled_heap_promise_keeps_result_alive_without_queued_jobs() {
        let mut heap = Heap::default();
        let mut scheduler = PromiseScheduler::default();
        let result = heap.allocate(HeapNode::Object(Vec::new())).unwrap();
        let promise = scheduler.create_heap_promise(&mut heap).unwrap();
        scheduler.settle_heap_promise(&mut heap, promise, Outcome::Fulfilled(VmValue::Reference(result))).unwrap();
        let snapshot = serde_json::to_vec(&(heap, scheduler)).unwrap();
        let (mut heap, _): (Heap, PromiseScheduler) = serde_json::from_slice(&snapshot).unwrap();
        heap.collect(&[VmValue::Reference(promise)]).unwrap();
        assert!(matches!(heap.get(result), Ok(HeapNode::Object(_))));
    }

    #[test]
    fn pending_heap_promise_keeps_subscriptions_after_restore() {
        let mut heap = Heap::default();
        let mut scheduler = PromiseScheduler::default();
        let promise = scheduler.create_heap_promise(&mut heap).unwrap();
        scheduler.subscribe_heap_promise(&mut heap, promise, ContinuationId(7)).unwrap();
        let snapshot = serde_json::to_vec(&(heap, scheduler)).unwrap();
        let (mut heap, mut scheduler): (Heap, PromiseScheduler) = serde_json::from_slice(&snapshot).unwrap();
        let failure = Outcome::Rejected(VmValue::String("failed".into()));
        assert!(scheduler.settle_heap_promise(&mut heap, promise, failure.clone()).unwrap());
        assert_eq!(scheduler.pop_job(), Some(Job { continuation: ContinuationId(7), outcome: failure }));
        assert!(scheduler.pop_job().is_none());
    }

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
