//! Durable recovery operations run independently of conversion workers and IPC.
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
#[cfg(test)]
use std::time::Duration;

use super::watcher::Shared;
use retype_core::error::{Code, Diagnostic, Error, Result};
use retype_core::execute::Cancellation;
use retype_core::job::{JobId, now_ms};
use retype_ipc::{RecoveryAction, RecoveryOperation, RecoveryState, Undone, WireError};

type Task = Box<dyn FnOnce(&Cancellation) -> Result<Option<Undone>> + Send>;
struct Work {
	operation: RecoveryOperation,
	cancellation: Cancellation,
	task: Task,
}
#[derive(Default)]
struct State {
	queue: VecDeque<Work>,
	running: BTreeMap<JobId, Cancellation>,
	closed: bool,
}

pub struct Pool {
	root: PathBuf,
	state: Mutex<State>,
	ready: Condvar,
	workers: Mutex<Vec<JoinHandle<()>>>,
}

impl Pool {
	pub fn start(state_dir: &Path) -> Result<Arc<Self>> {
		let root = state_dir.join("recovery");
		std::fs::create_dir_all(&root)?;
		let pool = Arc::new(Self {
			root,
			state: Mutex::new(State::default()),
			ready: Condvar::new(),
			workers: Mutex::new(Vec::new()),
		});
		for entry in std::fs::read_dir(&pool.root)? {
			let path = entry?.path();
			if path.extension().is_none_or(|ext| ext != "json") {
				continue;
			}
			let mut operation: RecoveryOperation =
				serde_json::from_slice(&std::fs::read(&path)?).map_err(|error| Error::Json(error.to_string()))?;
			if matches!(operation.state, RecoveryState::Queued | RecoveryState::Running) {
				operation.state = RecoveryState::Failed;
				operation.error = Some(WireError::new(
					Code::JobInterrupted,
					"recovery was interrupted; retained data remains protected; inspect and retry",
				));
				pool.record(&operation)?;
			}
		}
		for _ in 0..2 {
			let worker = Arc::clone(&pool);
			pool.workers
				.lock()
				.unwrap()
				.push(std::thread::spawn(move || worker.serve()));
		}
		Ok(pool)
	}

	fn record(&self, operation: &RecoveryOperation) -> Result<()> {
		let bytes = serde_json::to_vec(operation).map_err(|error| Error::Json(error.to_string()))?;
		retype_core::convert::write_atomically(&self.root.join(format!("{}.json", operation.id)), |path| {
			std::fs::write(path, &bytes)?;
			Ok(())
		})
	}

	pub fn submit(
		&self,
		job: JobId,
		action: RecoveryAction,
		task: impl FnOnce(&Cancellation) -> Result<Option<Undone>> + Send + 'static,
	) -> Result<RecoveryOperation> {
		self.submit_with_id(JobId::now()?, job, action, task)
	}

	pub fn submit_with_id(
		&self,
		id: JobId,
		job: JobId,
		action: RecoveryAction,
		task: impl FnOnce(&Cancellation) -> Result<Option<Undone>> + Send + 'static,
	) -> Result<RecoveryOperation> {
		let mut state = self
			.state
			.lock()
			.map_err(|_| std::io::Error::other("recovery queue lock poisoned"))?;
		if self.root.join(format!("{id}.json")).try_exists()? {
			return Err(Diagnostic::global(
				Code::JobState,
				"recovery handle already exists; inspect it before retrying",
			)
			.into());
		}
		if state.closed || state.queue.len() >= 256 {
			return Err(Diagnostic::global(
				Code::JobState,
				"recovery queue is full or stopping; no action was admitted",
			)
			.into());
		}
		let operation = RecoveryOperation {
			id,
			job,
			action,
			state: RecoveryState::Queued,
			updated: now_ms(),
			result: None,
			error: None,
		};
		self.record(&operation)?;
		state.queue.push_back(Work {
			operation: operation.clone(),
			cancellation: Cancellation::default(),
			task: Box::new(task),
		});
		self.ready.notify_one();
		Ok(operation)
	}

	pub fn get(&self, id: &JobId) -> Result<RecoveryOperation> {
		let bytes = std::fs::read(self.root.join(format!("{id}.json")))?;
		let mut operation: RecoveryOperation =
			serde_json::from_slice(&bytes).map_err(|error| Error::Json(error.to_string()))?;
		if matches!(operation.state, RecoveryState::Running | RecoveryState::Queued) {
			let state = self
				.state
				.lock()
				.map_err(|_| std::io::Error::other("recovery queue lock poisoned"))?;
			if !state.running.contains_key(id) && !state.queue.iter().any(|work| &work.operation.id == id) {
				operation.state = RecoveryState::Failed;
				operation.error = Some(WireError::new(
					Code::JobInterrupted,
					"recovery outcome could not be durably recorded; inspect retained data before retrying",
				));
			}
		}
		Ok(operation)
	}

	pub fn list(&self) -> Result<Vec<RecoveryOperation>> {
		let mut operations = Vec::new();
		for entry in std::fs::read_dir(&self.root)? {
			let path = entry?.path();
			if path.extension().is_none_or(|ext| ext != "json") {
				continue;
			}
			if let Some(id) = path
				.file_stem()
				.and_then(|name| name.to_str())
				.and_then(|id| JobId::parse(id).ok())
			{
				operations.push(self.get(&id)?);
			}
		}
		operations.sort_by_key(|operation| std::cmp::Reverse(operation.updated));
		Ok(operations)
	}

	pub fn cancel(&self, id: &JobId) -> Result<()> {
		let state = self
			.state
			.lock()
			.map_err(|_| std::io::Error::other("recovery queue lock poisoned"))?;
		let token = state.running.get(id).or_else(|| {
			state
				.queue
				.iter()
				.find(|work| work.operation.id == *id)
				.map(|work| &work.cancellation)
		});
		match token {
			Some(token) => {
				token.request_cancel();
				Ok(())
			}
			None => Err(Diagnostic::global(Code::JobState, "recovery is not queued or running").into()),
		}
	}

	pub fn idle(&self) -> bool {
		self.state
			.lock()
			.is_ok_and(|state| state.queue.is_empty() && state.running.is_empty())
	}

	pub fn close(&self, cancel: bool) {
		if let Ok(mut state) = self.state.lock() {
			state.closed = true;
			if cancel {
				for token in state
					.running
					.values()
					.chain(state.queue.iter().map(|work| &work.cancellation))
				{
					token.request_cancel();
				}
			}
		}
		self.ready.notify_all();
	}

	pub fn stop(&self) {
		self.close(false);
		let workers = std::mem::take(&mut *self.workers.lock().unwrap());
		for worker in workers {
			let _ = worker.join();
		}
	}

	#[cfg(test)]
	pub fn wait(&self) {
		let deadline = std::time::Instant::now() + Duration::from_secs(15);
		while !self.idle() {
			assert!(std::time::Instant::now() < deadline, "recovery did not settle");
			std::thread::sleep(Duration::from_millis(5));
		}
	}

	fn serve(&self) {
		loop {
			let work = {
				let mut state = self.state.lock().unwrap();
				loop {
					if let Some(work) = state.queue.pop_front() {
						state
							.running
							.insert(work.operation.id.clone(), work.cancellation.clone());
						break work;
					}
					if state.closed {
						return;
					}
					state = self.ready.wait(state).unwrap();
				}
			};
			let Work {
				mut operation,
				cancellation,
				task,
			} = work;
			operation.state = RecoveryState::Running;
			operation.updated = now_ms();
			let result = self
				.record(&operation)
				.and_then(|_| cancellation.commit(|| Ok(())))
				.and_then(|_| {
					std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(&cancellation))).unwrap_or_else(
						|_| {
							Err(Diagnostic::global(
								Code::InternalPanic,
								"recovery worker panicked; retained data remains protected",
							)
							.into())
						},
					)
				});
			match result {
				Ok(result) => {
					operation.state = RecoveryState::Done;
					operation.result = result;
				}
				Err(error) => {
					operation.state = if cancellation.is_cancelled() {
						RecoveryState::Cancelled
					} else {
						RecoveryState::Failed
					};
					operation.error = Some(WireError::new(error.code(), error.to_string()));
				}
			}
			operation.updated = now_ms();
			if let Err(error) = self.record(&operation) {
				eprintln!("retype: recovery {} result is not durable: {error}", operation.id);
			}
			self.state.lock().unwrap().running.remove(&operation.id);
		}
	}
}

pub fn cancel_cleanup(
	pool: &Pool,
	shared: Arc<Shared>,
	job: retype_core::job::Job,
	lease: retype_core::undo::JobLease,
) -> Result<RecoveryOperation> {
	pool.submit(job.id.clone(), RecoveryAction::Cancel, move |cancellation| {
		let store = retype_core::undo::Store::open(&shared.state_dir);
		let recovery = store.load(&job.id).and_then(|record| store.retain(lease, record));
		let mut job = job;
		let warning = super::cancel::finish_after_persist_cancellable(&shared, recovery, &mut job, cancellation);
		super::watcher::publish_persisted(&shared, &job);
		if let Some(warning) = warning {
			return Err(Diagnostic::global(Code::JobResumeUnavailable, warning).into());
		}
		Ok(None)
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn blocked_recovery_keeps_control_available_and_cancellation_is_durable() {
		let dir = tempfile::tempdir().unwrap();
		let pool = Pool::start(dir.path()).unwrap();
		let (entered, observed) = std::sync::mpsc::sync_channel(1);
		let operation = pool
			.submit(JobId::now().unwrap(), RecoveryAction::Undo, move |token| {
				entered.send(()).unwrap();
				while !token.is_cancelled() {
					std::thread::sleep(Duration::from_millis(5));
				}
				token.commit(|| Ok(None))
			})
			.unwrap();
		observed.recv_timeout(Duration::from_secs(2)).unwrap();
		let started = std::time::Instant::now();
		assert_eq!(pool.get(&operation.id).unwrap().state, RecoveryState::Running);
		pool.cancel(&operation.id).unwrap();
		pool.wait();
		assert!(started.elapsed() < Duration::from_secs(1));
		assert_eq!(pool.get(&operation.id).unwrap().state, RecoveryState::Cancelled);
		pool.stop();
		let reopened = Pool::start(dir.path()).unwrap();
		assert_eq!(reopened.get(&operation.id).unwrap().state, RecoveryState::Cancelled);
		reopened.stop();
	}
	#[test]
	fn a_panicking_recovery_is_reported_and_does_not_strand_its_worker_slot() {
		let dir = tempfile::tempdir().unwrap();
		let pool = Pool::start(dir.path()).unwrap();
		let operation = pool
			.submit(JobId::now().unwrap(), RecoveryAction::Cleanup, |_| {
				panic!("injected recovery panic")
			})
			.unwrap();
		pool.wait();
		let result = pool.get(&operation.id).unwrap();
		assert_eq!(result.state, RecoveryState::Failed);
		assert_eq!(result.error.unwrap().code, Code::InternalPanic);
		pool.stop();
	}

	#[test]
	fn a_completed_undo_result_outlives_the_recovery_copy() {
		let dir = tempfile::tempdir().unwrap();
		let pool = Pool::start(dir.path()).unwrap();
		let id = JobId::now().unwrap();
		let expected = Undone {
			id: id.clone(),
			source: dir.path().join("original"),
			consume_source: true,
			removed: vec![],
		};
		let result = expected.clone();
		let operation = pool
			.submit(id, RecoveryAction::Undo, move |_| Ok(Some(result)))
			.unwrap();
		pool.wait();
		assert_eq!(pool.get(&operation.id).unwrap().result, Some(expected));
		pool.stop();
	}
}
