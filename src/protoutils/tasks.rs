//! The tasks one accept or serve loop has running.
//!
//! Every loop in this crate hands the work it accepted off to its own task, so a slow or
//! broken stream cannot hold up the next one. Handing that task to `tokio::spawn` and
//! dropping the handle gives up the only two things the loop still needs: the count of what
//! is in flight, and any way to end those tasks once the loop is done with them. A `Tasks`
//! set is that handle - it counts, it refuses to take more work once the loop has reached
//! its limit, and it ends the running tasks when the loop stops.
//!
//! Dropping a `Tasks` without draining it aborts everything in it. That is the fallback for
//! a cancelled accept future: the set goes with the future and its tasks go with the set.

use core::time::Duration;

use tokio::task::JoinSet;
use tracing::{debug, warn};

/// How long a loop gives its in-flight tasks to finish on their own before aborting them.
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// The most tasks one loop may have running at a time.
///
/// Generous next to what one QUIC connection carries, so this is a ceiling against a peer
/// that opens streams faster than they close rather than a limit a normal session meets.
pub const MAX_INFLIGHT_TASKS: usize = 256;

/// The tasks one loop has running, reported under `label`.
#[derive(Debug)]
pub struct Tasks {
	label: &'static str,
	set: JoinSet<()>,
	limit: usize,
	drain_timeout: Duration,
}
impl Tasks {
	/// Creates an empty set.
	#[must_use]
	pub fn new(label: &'static str) -> Self {
		Self {
			label,
			set: JoinSet::new(),
			limit: MAX_INFLIGHT_TASKS,
			drain_timeout: DEFAULT_DRAIN_TIMEOUT,
		}
	}

	/// Overrides the point at which [`spawn`](Self::spawn) starts waiting for a free slot.
	#[must_use]
	pub fn with_limit(mut self, limit: usize) -> Self {
		self.limit = limit.max(1);
		self
	}

	/// Overrides how long [`drain`](Self::drain) waits before it aborts what is left.
	#[must_use]
	pub fn with_drain_timeout(mut self, drain_timeout: Duration) -> Self {
		self.drain_timeout = drain_timeout;
		self
	}

	/// How many tasks are running.
	#[must_use]
	pub fn in_flight(&self) -> usize {
		self.set.len()
	}

	/// Adds `task` to the set, waiting for a free slot first when the set is at its limit.
	///
	/// Waiting is the backpressure: a loop that has reached its limit stops accepting until
	/// one of the tasks it already took has finished.
	pub async fn spawn<F>(&mut self, task: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		while self.set.len() >= self.limit {
			debug!(
				"{}/{} {} tasks in flight, waiting for a free slot",
				self.set.len(),
				self.limit,
				self.label,
			);
			if self.set.join_next().await.is_none() {
				break;
			}
		}
		let _task = self.set.spawn(task);
	}

	/// Waits for the running tasks to finish on their own, then aborts whatever is left.
	///
	/// This is how a loop lets go of work that is still useful: the streams a connection
	/// left behind get their grace period, and only the ones that outlast it are reset.
	pub async fn drain(&mut self) {
		if self.set.is_empty() {
			return;
		}
		let (label, timeout) = (self.label, self.drain_timeout);
		debug!("draining {} {} tasks", self.set.len(), label);

		let drained = tokio::time::timeout(timeout, self.join_until_empty());
		if drained.await.is_err() {
			warn!(
				"aborting {} {} tasks that outlasted the {} s drain",
				self.set.len(),
				label,
				timeout.as_secs(),
			);
			self.set.abort_all();
			self.join_until_empty().await;
		}
	}

	/// Ends every running task now, and waits for each of them to actually stop.
	///
	/// This is the shutdown path: a proxy that has been asked to stop has no reason to let a
	/// forwarded connection keep going.
	pub async fn abort_all(&mut self) {
		if self.set.is_empty() {
			return;
		}
		debug!("aborting {} {} tasks", self.set.len(), self.label);
		self.set.abort_all();
		self.join_until_empty().await;
	}

	/// Joins tasks until the set is empty.
	async fn join_until_empty(&mut self) {
		while self.set.join_next().await.is_some() {}
	}
}

#[cfg(test)]
mod tests {
	use core::time::Duration;

	use color_eyre::eyre::Result;
	use tokio::sync::oneshot;
	use tokio_util::sync::CancellationToken;

	use super::Tasks;

	/// Reports on `tx` when the task holding it is dropped, which is how a test tells a
	/// task that finished apart from a task that was aborted.
	struct ReportOnDrop(Option<oneshot::Sender<()>>);

	impl Drop for ReportOnDrop {
		fn drop(&mut self) {
			if let Some(tx) = self.0.take() {
				let _sent = tx.send(());
			}
		}
	}

	/// A future that never completes, so the only way its task ends is being aborted.
	async fn pending_forever() {
		let token = CancellationToken::new();
		token.cancelled().await;
	}

	#[tokio::test]
	async fn drain_waits_for_a_task_that_finishes_on_its_own() -> Result<()> {
		let mut tasks = Tasks::new("drain");
		let (tx, rx) = oneshot::channel();

		tasks
			.spawn(async move {
				tokio::time::sleep(Duration::from_millis(20)).await;
				let _sent = tx.send(());
			})
			.await;
		assert_eq!(tasks.in_flight(), 1);

		tasks.drain().await;

		assert_eq!(tasks.in_flight(), 0);
		rx.await.expect("the task ran to its end");
		Ok(())
	}

	#[tokio::test]
	async fn drain_aborts_a_task_that_outlasts_the_grace_period() -> Result<()> {
		let mut tasks = Tasks::new("drain").with_drain_timeout(Duration::from_millis(50));
		let (tx, rx) = oneshot::channel();

		tasks
			.spawn(async move {
				let _report = ReportOnDrop(Some(tx));
				pending_forever().await;
			})
			.await;
		assert_eq!(tasks.in_flight(), 1);

		tasks.drain().await;

		assert_eq!(tasks.in_flight(), 0);
		rx.await
			.expect("the task was dropped, so its guard reported it");
		Ok(())
	}

	#[tokio::test]
	async fn spawn_waits_for_a_free_slot() -> Result<()> {
		let mut tasks = Tasks::new("limit").with_limit(1);
		let gate = CancellationToken::new();
		let held = gate.clone();

		tasks.spawn(async move { held.cancelled().await }).await;
		assert_eq!(tasks.in_flight(), 1);

		// The set is full, so the spawn never gets as far as spawning.
		let busy = tokio::time::timeout(Duration::from_millis(50), tasks.spawn(async {}));
		assert!(busy.await.is_err(), "a full set must not take another task");
		assert_eq!(tasks.in_flight(), 1);

		gate.cancel();
		tasks.drain().await;
		assert_eq!(tasks.in_flight(), 0);

		tasks.spawn(async {}).await;
		assert_eq!(tasks.in_flight(), 1);
		tasks.abort_all().await;
		assert_eq!(tasks.in_flight(), 0);
		Ok(())
	}

	#[tokio::test]
	async fn abort_all_ends_every_running_task() -> Result<()> {
		let mut tasks = Tasks::new("abort");
		let (first_start, first_started) = oneshot::channel();
		let (second_start, second_started) = oneshot::channel();
		let (first_tx, first_rx) = oneshot::channel();
		let (second_tx, second_rx) = oneshot::channel();

		for (start, report) in [(first_start, first_tx), (second_start, second_tx)] {
			tasks
				.spawn(async move {
					let _report = ReportOnDrop(Some(report));
					let _started = start.send(());
					pending_forever().await;
				})
				.await;
		}

		// Both have to have reached their body: a task that was spawned but never polled is
		// dropped without ever holding its guard, and that is not what this is testing.
		first_started.await.expect("the first task started");
		second_started.await.expect("the second task started");
		assert_eq!(tasks.in_flight(), 2);

		tasks.abort_all().await;

		assert_eq!(tasks.in_flight(), 0);
		first_rx.await.expect("the first task was dropped");
		second_rx.await.expect("the second task was dropped");
		Ok(())
	}
}
