// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Implements a `CandidateBackingSubsystem`.

#![deny(unused_crate_dependencies)]

use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::pin::Pin;
use std::sync::Arc;

use bitvec::vec::BitVec;
use futures::{channel::{mpsc, oneshot}, Future, FutureExt, SinkExt, StreamExt};

use sp_keystore::SyncCryptoStorePtr;
use polkadot_primitives::v1::{
	CommittedCandidateReceipt, BackedCandidate, Id as ParaId, ValidatorId,
	ValidatorIndex, SigningContext, PoV, CandidateHash,
	CandidateDescriptor, AvailableData, ValidatorSignature, Hash, CandidateReceipt,
	CandidateCommitments, CoreState, CoreIndex, CollatorId, ValidationOutputs,
};
use polkadot_node_primitives::{
	FromTableMisbehavior, Statement, SignedFullStatement, MisbehaviorReport, ValidationResult,
};
use polkadot_subsystem::{
	messages::{
		AllMessages, AvailabilityStoreMessage, CandidateBackingMessage, CandidateSelectionMessage,
		CandidateValidationMessage, NewBackedCandidate, PoVDistributionMessage, ProvisionableData,
		ProvisionerMessage, RuntimeApiMessage, StatementDistributionMessage, ValidationFailed,
		RuntimeApiRequest,
	},
};
use polkadot_node_subsystem_util::{
	self as util,
	request_session_index_for_child,
	request_validator_groups,
	request_validators,
	request_from_runtime,
	Validator,
	delegated_subsystem,
	metrics::{self, prometheus},
};
use statement_table::{
	generic::AttestedCandidate as TableAttestedCandidate,
	Context as TableContextTrait,
	Table,
	v1::{
		Statement as TableStatement,
		SignedStatement as TableSignedStatement, Summary as TableSummary,
	},
};
use thiserror::Error;

const LOG_TARGET: &str = "candidate_backing";

#[derive(Debug, Error)]
enum Error {
	#[error("Candidate is not found")]
	CandidateNotFound,
	#[error("Signature is invalid")]
	InvalidSignature,
	#[error("Failed to send candidates {0:?}")]
	Send(Vec<NewBackedCandidate>),
	#[error("Oneshot never resolved")]
	Oneshot(#[from] #[source] oneshot::Canceled),
	#[error("Obtaining erasure chunks failed")]
	ObtainErasureChunks(#[from] #[source] erasure_coding::Error),
	#[error(transparent)]
	ValidationFailed(#[from] ValidationFailed),
	#[error(transparent)]
	Mpsc(#[from] mpsc::SendError),
	#[error(transparent)]
	UtilError(#[from] util::Error),
}

/// Holds all data needed for candidate backing job operation.
struct CandidateBackingJob {
	/// The hash of the relay parent on top of which this job is doing it's work.
	parent: Hash,
	/// Inbound message channel receiving part.
	rx_to: mpsc::Receiver<ToJob>,
	/// Outbound message channel sending part.
	tx_from: mpsc::Sender<FromJob>,
	/// The `ParaId` assigned to this validator
	assignment: ParaId,
	/// The collator required to author the candidate, if any.
	required_collator: Option<CollatorId>,
	/// We issued `Valid` or `Invalid` statements on about these candidates.
	issued_statements: HashSet<CandidateHash>,
	/// `Some(h)` if this job has already issues `Seconded` statemt for some candidate with `h` hash.
	seconded: Option<CandidateHash>,
	/// The candidates that are includable, by hash. Each entry here indicates
	/// that we've sent the provisioner the backed candidate.
	backed: HashSet<CandidateHash>,
	/// We have already reported misbehaviors for these validators.
	reported_misbehavior_for: HashSet<ValidatorIndex>,
	keystore: SyncCryptoStorePtr,
	table: Table<TableContext>,
	table_context: TableContext,
	metrics: Metrics,
}

const fn group_quorum(n_validators: usize) -> usize {
	(n_validators / 2) + 1
}

#[derive(Default)]
struct TableContext {
	signing_context: SigningContext,
	validator: Option<Validator>,
	groups: HashMap<ParaId, Vec<ValidatorIndex>>,
	validators: Vec<ValidatorId>,
}

impl TableContextTrait for TableContext {
	type AuthorityId = ValidatorIndex;
	type Digest = CandidateHash;
	type GroupId = ParaId;
	type Signature = ValidatorSignature;
	type Candidate = CommittedCandidateReceipt;

	fn candidate_digest(candidate: &CommittedCandidateReceipt) -> CandidateHash {
		candidate.hash()
	}

	fn candidate_group(candidate: &CommittedCandidateReceipt) -> ParaId {
		candidate.descriptor().para_id
	}

	fn is_member_of(&self, authority: &ValidatorIndex, group: &ParaId) -> bool {
		self.groups.get(group).map_or(false, |g| g.iter().position(|a| a == authority).is_some())
	}

	fn requisite_votes(&self, group: &ParaId) -> usize {
		self.groups.get(group).map_or(usize::max_value(), |g| group_quorum(g.len()))
	}
}

/// A message type that is sent from `CandidateBackingSubsystem` to `CandidateBackingJob`.
pub enum ToJob {
	/// A `CandidateBackingMessage`.
	CandidateBacking(CandidateBackingMessage),
	/// Stop working.
	Stop,
}

impl TryFrom<AllMessages> for ToJob {
	type Error = ();

	fn try_from(msg: AllMessages) -> Result<Self, Self::Error> {
		match msg {
			AllMessages::CandidateBacking(msg) => Ok(ToJob::CandidateBacking(msg)),
			_ => Err(()),
		}
	}
}

impl From<CandidateBackingMessage> for ToJob {
	fn from(msg: CandidateBackingMessage) -> Self {
		Self::CandidateBacking(msg)
	}
}

impl util::ToJobTrait for ToJob {
	const STOP: Self = ToJob::Stop;

	fn relay_parent(&self) -> Option<Hash> {
		match self {
			Self::CandidateBacking(cb) => cb.relay_parent(),
			Self::Stop => None,
		}
	}
}

/// A message type that is sent from `CandidateBackingJob` to `CandidateBackingSubsystem`.
enum FromJob {
	AvailabilityStore(AvailabilityStoreMessage),
	RuntimeApiMessage(RuntimeApiMessage),
	CandidateValidation(CandidateValidationMessage),
	CandidateSelection(CandidateSelectionMessage),
	Provisioner(ProvisionerMessage),
	PoVDistribution(PoVDistributionMessage),
	StatementDistribution(StatementDistributionMessage),
}

impl From<FromJob> for AllMessages {
	fn from(f: FromJob) -> Self {
		match f {
			FromJob::AvailabilityStore(msg) => AllMessages::AvailabilityStore(msg),
			FromJob::RuntimeApiMessage(msg) => AllMessages::RuntimeApi(msg),
			FromJob::CandidateValidation(msg) => AllMessages::CandidateValidation(msg),
			FromJob::CandidateSelection(msg) => AllMessages::CandidateSelection(msg),
			FromJob::StatementDistribution(msg) => AllMessages::StatementDistribution(msg),
			FromJob::PoVDistribution(msg) => AllMessages::PoVDistribution(msg),
			FromJob::Provisioner(msg) => AllMessages::Provisioner(msg),
		}
	}
}

impl TryFrom<AllMessages> for FromJob {
	type Error = &'static str;

	fn try_from(f: AllMessages) -> Result<Self, Self::Error> {
		match f {
			AllMessages::AvailabilityStore(msg) => Ok(FromJob::AvailabilityStore(msg)),
			AllMessages::RuntimeApi(msg) => Ok(FromJob::RuntimeApiMessage(msg)),
			AllMessages::CandidateValidation(msg) => Ok(FromJob::CandidateValidation(msg)),
			AllMessages::CandidateSelection(msg) => Ok(FromJob::CandidateSelection(msg)),
			AllMessages::StatementDistribution(msg) => Ok(FromJob::StatementDistribution(msg)),
			AllMessages::PoVDistribution(msg) => Ok(FromJob::PoVDistribution(msg)),
			AllMessages::Provisioner(msg) => Ok(FromJob::Provisioner(msg)),
			_ => Err("can't convert this AllMessages variant to FromJob"),
		}
	}
}

// It looks like it's not possible to do an `impl From` given the current state of
// the code. So this does the necessary conversion.
fn primitive_statement_to_table(s: &SignedFullStatement) -> TableSignedStatement {
	let statement = match s.payload() {
		Statement::Seconded(c) => TableStatement::Candidate(c.clone()),
		Statement::Valid(h) => TableStatement::Valid(h.clone()),
		Statement::Invalid(h) => TableStatement::Invalid(h.clone()),
	};

	TableSignedStatement {
		statement,
		signature: s.signature().clone(),
		sender: s.validator_index(),
	}
}

#[tracing::instrument(level = "trace", skip(attested, table_context), fields(subsystem = LOG_TARGET))]
fn table_attested_to_backed(
	attested: TableAttestedCandidate<
		ParaId,
		CommittedCandidateReceipt,
		ValidatorIndex,
		ValidatorSignature,
	>,
	table_context: &TableContext,
) -> Option<BackedCandidate> {
	let TableAttestedCandidate { candidate, validity_votes, group_id: para_id } = attested;

	let (ids, validity_votes): (Vec<_>, Vec<_>) = validity_votes
		.into_iter()
		.map(|(id, vote)| (id, vote.into()))
		.unzip();

	let group = table_context.groups.get(&para_id)?;

	let mut validator_indices = BitVec::with_capacity(group.len());

	validator_indices.resize(group.len(), false);

	for id in ids.iter() {
		if let Some(position) = group.iter().position(|x| x == id) {
			validator_indices.set(position, true);
		}
	}

	Some(BackedCandidate {
		candidate,
		validity_votes,
		validator_indices,
	})
}

impl CandidateBackingJob {
	/// Run asynchronously.
	async fn run_loop(mut self) -> Result<(), Error> {
		while let Some(msg) = self.rx_to.next().await {
			match msg {
				ToJob::CandidateBacking(msg) => {
					self.process_msg(msg).await?;
				}
				ToJob::Stop => break,
			}
		}

		Ok(())
	}

	async fn issue_candidate_invalid_message(
		&mut self,
		candidate: CandidateReceipt,
	) -> Result<(), Error> {
		self.tx_from.send(FromJob::CandidateSelection(
			CandidateSelectionMessage::Invalid(self.parent, candidate)
		)).await?;

		Ok(())
	}

	/// Validate the candidate that is requested to be `Second`ed and distribute validation result.
	///
	/// Returns `Ok(true)` if we issued a `Seconded` statement about this candidate.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn validate_and_second(
		&mut self,
		candidate: &CandidateReceipt,
		pov: Arc<PoV>,
	) -> Result<bool, Error> {
		// Check that candidate is collated by the right collator.
		if self.required_collator.as_ref()
			.map_or(false, |c| c != &candidate.descriptor().collator)
		{
			self.issue_candidate_invalid_message(candidate.clone()).await?;
			return Ok(false);
		}

		let valid = self.request_candidate_validation(
			candidate.descriptor().clone(),
			pov.clone(),
		).await?;

		let candidate_hash = candidate.hash();

		let statement = match valid {
			ValidationResult::Valid(outputs, validation_data) => {
				// make PoV available for later distribution. Send data to the availability
				// store to keep. Sign and dispatch `valid` statement to network if we
				// have not seconded the given candidate.
				//
				// If the commitments hash produced by validation is not the same as given by
				// the collator, do not make available and report the collator.
				let commitments_check = self.make_pov_available(
					pov,
					candidate_hash,
					validation_data,
					outputs,
					|commitments| if commitments.hash() == candidate.commitments_hash {
						Ok(CommittedCandidateReceipt {
							descriptor: candidate.descriptor().clone(),
							commitments,
						})
					} else {
						Err(())
					},
				).await?;

				match commitments_check {
					Ok(candidate) => {
						self.issued_statements.insert(candidate_hash);
						Some(Statement::Seconded(candidate))
					}
					Err(()) => {
						self.issue_candidate_invalid_message(candidate.clone()).await?;
						None
					}
				}
			}
			ValidationResult::Invalid(_reason) => {
				// no need to issue a statement about this if we aren't seconding it.
				//
				// there's an infinite amount of garbage out there. no need to acknowledge
				// all of it.
				self.issue_candidate_invalid_message(candidate.clone()).await?;
				None
			}
		};

		let issued_statement = statement.is_some();

		if let Some(statement) = statement {
			self.sign_import_and_distribute_statement(statement).await?
		}

		Ok(issued_statement)
	}

	async fn sign_import_and_distribute_statement(&mut self, statement: Statement) -> Result<(), Error> {
		if let Some(signed_statement) = self.sign_statement(statement).await {
			self.import_statement(&signed_statement).await?;
			self.distribute_signed_statement(signed_statement).await?;
		}

		Ok(())
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn get_backed(&self) -> Vec<NewBackedCandidate> {
		let proposed = self.table.proposed_candidates(&self.table_context);
		let mut res = Vec::with_capacity(proposed.len());

		for p in proposed.into_iter() {
			match table_attested_to_backed(p, &self.table_context) {
				None => continue,
				Some(backed) => res.push(NewBackedCandidate(backed)),
			}
		}

		res
	}

	/// Check if there have happened any new misbehaviors and issue necessary messages.
	///
	/// TODO: Report multiple misbehaviors (https://github.com/paritytech/polkadot/issues/1387)
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn issue_new_misbehaviors(&mut self) -> Result<(), Error> {
		let mut reports = Vec::new();

		for (k, v) in self.table.get_misbehavior().iter() {
			if !self.reported_misbehavior_for.contains(k) {
				self.reported_misbehavior_for.insert(*k);

				let f = FromTableMisbehavior {
					id: *k,
					report: v.clone(),
					signing_context: self.table_context.signing_context.clone(),
					key: self.table_context.validators[*k as usize].clone(),
				};

				if let Ok(report) = MisbehaviorReport::try_from(f) {
					let message = ProvisionerMessage::ProvisionableData(
						self.parent,
						ProvisionableData::MisbehaviorReport(self.parent, report),
					);

					reports.push(message);
				}
			}
		}

		for report in reports.drain(..) {
			self.send_to_provisioner(report).await?
		}

		Ok(())
	}

	/// Import a statement into the statement table and return the summary of the import.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn import_statement(
		&mut self,
		statement: &SignedFullStatement,
	) -> Result<Option<TableSummary>, Error> {
		let stmt = primitive_statement_to_table(statement);

		let summary = self.table.import_statement(&self.table_context, stmt);

		if let Some(ref summary) = summary {
			if let Some(attested) = self.table.attested_candidate(
				&summary.candidate,
				&self.table_context,
			) {
				// `HashSet::insert` returns true if the thing wasn't in there already.
				// one of the few places the Rust-std folks did a bad job with API
				if self.backed.insert(summary.candidate) {
					if let Some(backed) =
						table_attested_to_backed(attested, &self.table_context)
					{
						let message = ProvisionerMessage::ProvisionableData(
							self.parent,
							ProvisionableData::BackedCandidate(backed),
						);
						self.send_to_provisioner(message).await?;
					}
				}
			}
		}

		self.issue_new_misbehaviors().await?;

		Ok(summary)
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn process_msg(&mut self, msg: CandidateBackingMessage) -> Result<(), Error> {

		match msg {
			CandidateBackingMessage::Second(_, candidate, pov) => {
				let _timer = self.metrics.time_process_second();

				// Sanity check that candidate is from our assignment.
				if candidate.descriptor().para_id != self.assignment {
					return Ok(());
				}

				// If the message is a `CandidateBackingMessage::Second`, sign and dispatch a
				// Seconded statement only if we have not seconded any other candidate and
				// have not signed a Valid statement for the requested candidate.
				if self.seconded.is_none() {
					// This job has not seconded a candidate yet.
					let candidate_hash = candidate.hash();
					let pov = Arc::new(pov);

					if !self.issued_statements.contains(&candidate_hash) {
						if let Ok(true) = self.validate_and_second(
							&candidate,
							pov.clone(),
						).await {
							self.metrics.on_candidate_seconded();
							self.seconded = Some(candidate_hash);
							self.distribute_pov(candidate.descriptor, pov).await?;
						}
					}
				}
			}
			CandidateBackingMessage::Statement(_, statement) => {
				let _timer = self.metrics.time_process_statement();

				self.check_statement_signature(&statement)?;
				match self.maybe_validate_and_import(statement).await {
					Err(Error::ValidationFailed(_)) => return Ok(()),
					Err(e) => return Err(e),
					Ok(()) => (),
				}
			}
			CandidateBackingMessage::GetBackedCandidates(_, tx) => {
				let _timer = self.metrics.time_get_backed_candidates();

				let backed = self.get_backed();

				tx.send(backed).map_err(|data| Error::Send(data))?;
			}
		}

		Ok(())
	}

	/// Kick off validation work and distribute the result as a signed statement.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn kick_off_validation_work(
		&mut self,
		summary: TableSummary,
	) -> Result<(), Error> {
		let candidate_hash = summary.candidate;

		if self.issued_statements.contains(&candidate_hash) {
			return Ok(())
		}

		// We clone the commitments here because there are borrowck
		// errors relating to this being a struct and methods borrowing the entirety of self
		// and not just those things that the function uses.
		let candidate = self.table.get_candidate(&candidate_hash).ok_or(Error::CandidateNotFound)?;
		let expected_commitments = candidate.commitments.clone();

		let descriptor = candidate.descriptor().clone();

		// Check that candidate is collated by the right collator.
		if self.required_collator.as_ref()
			.map_or(false, |c| c != &descriptor.collator)
		{
			// If not, we've got the statement in the table but we will
			// not issue validation work for it.
			//
			// Act as though we've issued a statement.
			self.issued_statements.insert(candidate_hash);
			return Ok(());
		}

		let pov = self.request_pov_from_distribution(descriptor.clone()).await?;
		let v = self.request_candidate_validation(descriptor, pov.clone()).await?;

		let statement = match v {
			ValidationResult::Valid(outputs, validation_data) => {
				// If validation produces a new set of commitments, we vote the candidate as invalid.
				let commitments_check = self.make_pov_available(
					pov,
					candidate_hash,
					validation_data,
					outputs,
					|commitments| if commitments == expected_commitments {
						Ok(())
					} else {
						Err(())
					}
				).await?;

				match commitments_check {
					Ok(()) => Statement::Valid(candidate_hash),
					Err(()) => Statement::Invalid(candidate_hash),
				}
			}
			ValidationResult::Invalid(_reason) => {
				Statement::Invalid(candidate_hash)
			}
		};

		self.issued_statements.insert(candidate_hash);

		self.sign_import_and_distribute_statement(statement).await
	}

	/// Import the statement and kick off validation work if it is a part of our assignment.
	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn maybe_validate_and_import(
		&mut self,
		statement: SignedFullStatement,
	) -> Result<(), Error> {
		if let Some(summary) = self.import_statement(&statement).await? {
			if let Statement::Seconded(_) = statement.payload() {
				if summary.group_id == self.assignment {
					self.kick_off_validation_work(summary).await?;
				}
			}
		}

		Ok(())
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	async fn sign_statement(&self, statement: Statement) -> Option<SignedFullStatement> {
		let signed = self.table_context
			.validator
			.as_ref()?
			.sign(self.keystore.clone(), statement)
			.await
			.ok()?;
		self.metrics.on_statement_signed();
		Some(signed)
	}

	#[tracing::instrument(level = "trace", skip(self), fields(subsystem = LOG_TARGET))]
	fn check_statement_signature(&self, statement: &SignedFullStatement) -> Result<(), Error> {
		let idx = statement.validator_index() as usize;

		if self.table_context.validators.len() > idx {
			statement.check_signature(
				&self.table_context.signing_context,
				&self.table_context.validators[idx],
			).map_err(|_| Error::InvalidSignature)?;
		} else {
			return Err(Error::InvalidSignature);
		}

		Ok(())
	}

	async fn send_to_provisioner(&mut self, msg: ProvisionerMessage) -> Result<(), Error> {
		self.tx_from.send(FromJob::Provisioner(msg)).await?;

		Ok(())
	}

	async fn distribute_pov(
		&mut self,
		descriptor: CandidateDescriptor,
		pov: Arc<PoV>,
	) -> Result<(), Error> {
		self.tx_from.send(FromJob::PoVDistribution(
			PoVDistributionMessage::DistributePoV(self.parent, descriptor, pov),
		)).await.map_err(Into::into)
	}

	async fn request_pov_from_distribution(
		&mut self,
		descriptor: CandidateDescriptor,
	) -> Result<Arc<PoV>, Error> {
		let (tx, rx) = oneshot::channel();

		self.tx_from.send(FromJob::PoVDistribution(
			PoVDistributionMessage::FetchPoV(self.parent, descriptor, tx)
		)).await?;

		Ok(rx.await?)
	}

	async fn request_candidate_validation(
		&mut self,
		candidate: CandidateDescriptor,
		pov: Arc<PoV>,
	) -> Result<ValidationResult, Error> {
		let (tx, rx) = oneshot::channel();

		self.tx_from.send(FromJob::CandidateValidation(
				CandidateValidationMessage::ValidateFromChainState(
					candidate,
					pov,
					tx,
				)
			)
		).await?;

		Ok(rx.await??)
	}

	async fn store_available_data(
		&mut self,
		id: Option<ValidatorIndex>,
		n_validators: u32,
		candidate_hash: CandidateHash,
		available_data: AvailableData,
	) -> Result<(), Error> {
		let (tx, rx) = oneshot::channel();
		self.tx_from.send(FromJob::AvailabilityStore(
				AvailabilityStoreMessage::StoreAvailableData(
					candidate_hash,
					id,
					n_validators,
					available_data,
					tx,
				)
			)
		).await?;

		let _ = rx.await?;

		Ok(())
	}

	// Make a `PoV` available.
	//
	// This calls an inspection function before making the PoV available for any last checks
	// that need to be done. If the inspection function returns an error, this function returns
	// early without making the PoV available.
	#[tracing::instrument(level = "trace", skip(self, pov, with_commitments), fields(subsystem = LOG_TARGET))]
	async fn make_pov_available<T, E>(
		&mut self,
		pov: Arc<PoV>,
		candidate_hash: CandidateHash,
		validation_data: polkadot_primitives::v1::PersistedValidationData,
		outputs: ValidationOutputs,
		with_commitments: impl FnOnce(CandidateCommitments) -> Result<T, E>,
	) -> Result<Result<T, E>, Error> {
		let available_data = AvailableData {
			pov,
			validation_data,
		};

		let chunks = erasure_coding::obtain_chunks_v1(
			self.table_context.validators.len(),
			&available_data,
		)?;

		let branches = erasure_coding::branches(chunks.as_ref());
		let erasure_root = branches.root();

		let commitments = CandidateCommitments {
			upward_messages: outputs.upward_messages,
			horizontal_messages: outputs.horizontal_messages,
			erasure_root,
			new_validation_code: outputs.new_validation_code,
			head_data: outputs.head_data,
			processed_downward_messages: outputs.processed_downward_messages,
			hrmp_watermark: outputs.hrmp_watermark,
		};

		let res = match with_commitments(commitments) {
			Ok(x) => x,
			Err(e) => return Ok(Err(e)),
		};

		self.store_available_data(
			self.table_context.validator.as_ref().map(|v| v.index()),
			self.table_context.validators.len() as u32,
			candidate_hash,
			available_data,
		).await?;

		Ok(Ok(res))
	}

	async fn distribute_signed_statement(&mut self, s: SignedFullStatement) -> Result<(), Error> {
		let smsg = StatementDistributionMessage::Share(self.parent, s);

		self.tx_from.send(FromJob::StatementDistribution(smsg)).await?;

		Ok(())
	}
}

impl util::JobTrait for CandidateBackingJob {
	type ToJob = ToJob;
	type FromJob = FromJob;
	type Error = Error;
	type RunArgs = SyncCryptoStorePtr;
	type Metrics = Metrics;

	const NAME: &'static str = "CandidateBackingJob";

	#[tracing::instrument(skip(keystore, metrics, rx_to, tx_from), fields(subsystem = LOG_TARGET))]
	fn run(
		parent: Hash,
		keystore: SyncCryptoStorePtr,
		metrics: Metrics,
		rx_to: mpsc::Receiver<Self::ToJob>,
		mut tx_from: mpsc::Sender<Self::FromJob>,
	) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>> {
		async move {
			macro_rules! try_runtime_api {
				($x: expr) => {
					match $x {
						Ok(x) => x,
						Err(e) => {
							tracing::warn!(
								target: LOG_TARGET,
								err = ?e,
								"Failed to fetch runtime API data for job",
							);

							// We can't do candidate validation work if we don't have the
							// requisite runtime API data. But these errors should not take
							// down the node.
							return Ok(());
						}
					}
				}
			}

			let (validators, groups, session_index, cores) = futures::try_join!(
				request_validators(parent, &mut tx_from).await?,
				request_validator_groups(parent, &mut tx_from).await?,
				request_session_index_for_child(parent, &mut tx_from).await?,
				request_from_runtime(
					parent,
					&mut tx_from,
					|tx| RuntimeApiRequest::AvailabilityCores(tx),
				).await?,
			)?;

			let validators = try_runtime_api!(validators);
			let (validator_groups, group_rotation_info) = try_runtime_api!(groups);
			let session_index = try_runtime_api!(session_index);
			let cores = try_runtime_api!(cores);

			let signing_context = SigningContext { parent_hash: parent, session_index };
			let validator = match Validator::construct(
				&validators,
				signing_context,
				keystore.clone(),
			).await {
				Ok(v) => v,
				Err(util::Error::NotAValidator) => { return Ok(()) },
				Err(e) => {
					tracing::warn!(
						target: LOG_TARGET,
						err = ?e,
						"Cannot participate in candidate backing",
					);

					return Ok(())
				}
			};

			let mut groups = HashMap::new();

			let n_cores = cores.len();

			let mut assignment = None;
			for (idx, core) in cores.into_iter().enumerate() {
				// Ignore prospective assignments on occupied cores for the time being.
				if let CoreState::Scheduled(scheduled) = core {
					let core_index = CoreIndex(idx as _);
					let group_index = group_rotation_info.group_for_core(core_index, n_cores);
					if let Some(g) = validator_groups.get(group_index.0 as usize) {
						if g.contains(&validator.index()) {
							assignment = Some((scheduled.para_id, scheduled.collator));
						}
						groups.insert(scheduled.para_id, g.clone());
					}
				}
			}

			let table_context = TableContext {
				groups,
				validators,
				signing_context: validator.signing_context().clone(),
				validator: Some(validator),
			};

			let (assignment, required_collator) = match assignment {
				None => return Ok(()), // no need to work.
				Some(r) => r,
			};

			let job = CandidateBackingJob {
				parent,
				rx_to,
				tx_from,
				assignment,
				required_collator,
				issued_statements: HashSet::new(),
				seconded: None,
				backed: HashSet::new(),
				reported_misbehavior_for: HashSet::new(),
				keystore,
				table: Table::default(),
				table_context,
				metrics,
			};

			job.run_loop().await
		}
		.boxed()
	}
}

#[derive(Clone)]
struct MetricsInner {
	signed_statements_total: prometheus::Counter<prometheus::U64>,
	candidates_seconded_total: prometheus::Counter<prometheus::U64>,
	process_second: prometheus::Histogram,
	process_statement: prometheus::Histogram,
	get_backed_candidates: prometheus::Histogram,
}

/// Candidate backing metrics.
#[derive(Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

impl Metrics {
	fn on_statement_signed(&self) {
		if let Some(metrics) = &self.0 {
			metrics.signed_statements_total.inc();
		}
	}

	fn on_candidate_seconded(&self) {
		if let Some(metrics) = &self.0 {
			metrics.candidates_seconded_total.inc();
		}
	}

	/// Provide a timer for handling `CandidateBackingMessage:Second` which observes on drop.
	fn time_process_second(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.process_second.start_timer())
	}

	/// Provide a timer for handling `CandidateBackingMessage::Statement` which observes on drop.
	fn time_process_statement(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.process_statement.start_timer())
	}

	/// Provide a timer for handling `CandidateBackingMessage::GetBackedCandidates` which observes on drop.
	fn time_get_backed_candidates(&self) -> Option<metrics::prometheus::prometheus::HistogramTimer> {
		self.0.as_ref().map(|metrics| metrics.get_backed_candidates.start_timer())
	}
}

impl metrics::Metrics for Metrics {
	fn try_register(registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		let metrics = MetricsInner {
			signed_statements_total: prometheus::register(
				prometheus::Counter::new(
					"parachain_candidate_backing_signed_statements_total",
					"Number of statements signed.",
				)?,
				registry,
			)?,
			candidates_seconded_total: prometheus::register(
				prometheus::Counter::new(
					"parachain_candidate_backing_candidates_seconded_total",
					"Number of candidates seconded.",
				)?,
				registry,
			)?,
			process_second: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_candidate_backing_process_second",
						"Time spent within `candidate_backing::process_second`",
					)
				)?,
				registry,
			)?,
			process_statement: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_candidate_backing_process_statement",
						"Time spent within `candidate_backing::process_statement`",
					)
				)?,
				registry,
			)?,
			get_backed_candidates: prometheus::register(
				prometheus::Histogram::with_opts(
					prometheus::HistogramOpts::new(
						"parachain_candidate_backing_get_backed_candidates",
						"Time spent within `candidate_backing::get_backed_candidates`",
					)
				)?,
				registry,
			)?,
		};
		Ok(Metrics(Some(metrics)))
	}
}

delegated_subsystem!(CandidateBackingJob(SyncCryptoStorePtr, Metrics) <- ToJob as CandidateBackingSubsystem);

#[cfg(test)]
mod tests {
	use super::*;
	use assert_matches::assert_matches;
	use futures::{future, Future};
	use polkadot_primitives::v1::{
		ScheduledCore, BlockData, CandidateCommitments,
		PersistedValidationData, ValidationData, TransientValidationData, HeadData,
		ValidityAttestation, GroupRotationInfo,
	};
	use polkadot_subsystem::{
		messages::RuntimeApiRequest,
		ActiveLeavesUpdate, FromOverseer, OverseerSignal,
	};
	use polkadot_node_primitives::InvalidCandidate;
	use sp_keyring::Sr25519Keyring;
	use sp_application_crypto::AppKey;
	use sp_keystore::{CryptoStore, SyncCryptoStore};
	use std::collections::HashMap;

	fn validator_pubkeys(val_ids: &[Sr25519Keyring]) -> Vec<ValidatorId> {
		val_ids.iter().map(|v| v.public().into()).collect()
	}

	struct TestState {
		chain_ids: Vec<ParaId>,
		keystore: SyncCryptoStorePtr,
		validators: Vec<Sr25519Keyring>,
		validator_public: Vec<ValidatorId>,
		validation_data: ValidationData,
		validator_groups: (Vec<Vec<ValidatorIndex>>, GroupRotationInfo),
		availability_cores: Vec<CoreState>,
		head_data: HashMap<ParaId, HeadData>,
		signing_context: SigningContext,
		relay_parent: Hash,
	}

	impl Default for TestState {
		fn default() -> Self {
			let chain_a = ParaId::from(1);
			let chain_b = ParaId::from(2);
			let thread_a = ParaId::from(3);

			let chain_ids = vec![chain_a, chain_b, thread_a];

			let validators = vec![
				Sr25519Keyring::Alice,
				Sr25519Keyring::Bob,
				Sr25519Keyring::Charlie,
				Sr25519Keyring::Dave,
				Sr25519Keyring::Ferdie,
				Sr25519Keyring::One,
			];

			let keystore = Arc::new(sc_keystore::LocalKeystore::in_memory());
			// Make sure `Alice` key is in the keystore, so this mocked node will be a parachain validator.
			SyncCryptoStore::sr25519_generate_new(&*keystore, ValidatorId::ID, Some(&validators[0].to_seed()))
				.expect("Insert key into keystore");

			let validator_public = validator_pubkeys(&validators);

			let validator_groups = vec![vec![2, 0, 3, 5], vec![1], vec![4]];
			let group_rotation_info = GroupRotationInfo {
				session_start_block: 0,
				group_rotation_frequency: 100,
				now: 1,
			};

			let thread_collator: CollatorId = Sr25519Keyring::Two.public().into();
			let availability_cores = vec![
				CoreState::Scheduled(ScheduledCore {
					para_id: chain_a,
					collator: None,
				}),
				CoreState::Scheduled(ScheduledCore {
					para_id: chain_b,
					collator: None,
				}),
				CoreState::Scheduled(ScheduledCore {
					para_id: thread_a,
					collator: Some(thread_collator.clone()),
				}),
			];

			let mut head_data = HashMap::new();
			head_data.insert(chain_a, HeadData(vec![4, 5, 6]));

			let relay_parent = Hash::from([5; 32]);

			let signing_context = SigningContext {
				session_index: 1,
				parent_hash: relay_parent,
			};

			let validation_data = ValidationData {
				persisted: PersistedValidationData {
					parent_head: HeadData(vec![7, 8, 9]),
					block_number: Default::default(),
					hrmp_mqc_heads: Vec::new(),
					dmq_mqc_head: Default::default(),
					max_pov_size: 1024,
				},
				transient: TransientValidationData {
					max_code_size: 1000,
					max_head_data_size: 1000,
					balance: Default::default(),
					code_upgrade_allowed: None,
					dmq_length: 0,
				},
			};

			Self {
				chain_ids,
				keystore,
				validators,
				validator_public,
				validator_groups: (validator_groups, group_rotation_info),
				availability_cores,
				head_data,
				validation_data,
				signing_context,
				relay_parent,
			}
		}
	}

	struct TestHarness {
		virtual_overseer: polkadot_node_subsystem_test_helpers::TestSubsystemContextHandle<CandidateBackingMessage>,
	}

	fn test_harness<T: Future<Output=()>>(keystore: SyncCryptoStorePtr, test: impl FnOnce(TestHarness) -> T) {
		let pool = sp_core::testing::TaskExecutor::new();

		let (context, virtual_overseer) = polkadot_node_subsystem_test_helpers::make_subsystem_context(pool.clone());

		let subsystem = CandidateBackingSubsystem::run(context, keystore, Metrics(None), pool.clone());

		let test_fut = test(TestHarness {
			virtual_overseer,
		});

		futures::pin_mut!(test_fut);
		futures::pin_mut!(subsystem);
		futures::executor::block_on(future::select(test_fut, subsystem));
	}

	fn make_erasure_root(test: &TestState, pov: PoV) -> Hash {
		let available_data = AvailableData {
			validation_data: test.validation_data.persisted.clone(),
			pov: Arc::new(pov),
		};

		let chunks = erasure_coding::obtain_chunks_v1(test.validators.len(), &available_data).unwrap();
		erasure_coding::branches(&chunks).root()
	}

	#[derive(Default)]
	struct TestCandidateBuilder {
		para_id: ParaId,
		head_data: HeadData,
		pov_hash: Hash,
		relay_parent: Hash,
		erasure_root: Hash,
	}

	impl TestCandidateBuilder {
		fn build(self) -> CommittedCandidateReceipt {
			CommittedCandidateReceipt {
				descriptor: CandidateDescriptor {
					para_id: self.para_id,
					pov_hash: self.pov_hash,
					relay_parent: self.relay_parent,
					..Default::default()
				},
				commitments: CandidateCommitments {
					head_data: self.head_data,
					erasure_root: self.erasure_root,
					..Default::default()
				},
			}
		}
	}

	// Tests that the subsystem performs actions that are requied on startup.
	async fn test_startup(
		virtual_overseer: &mut polkadot_node_subsystem_test_helpers::TestSubsystemContextHandle<CandidateBackingMessage>,
		test_state: &TestState,
	) {
		// Start work on some new parent.
		virtual_overseer.send(FromOverseer::Signal(
			OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::start_work(test_state.relay_parent)))
		).await;

		// Check that subsystem job issues a request for a validator set.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::Validators(tx))
			) if parent == test_state.relay_parent => {
				tx.send(Ok(test_state.validator_public.clone())).unwrap();
			}
		);

		// Check that subsystem job issues a request for the validator groups.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::ValidatorGroups(tx))
			) if parent == test_state.relay_parent => {
				tx.send(Ok(test_state.validator_groups.clone())).unwrap();
			}
		);

		// Check that subsystem job issues a request for the session index for child.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::SessionIndexForChild(tx))
			) if parent == test_state.relay_parent => {
				tx.send(Ok(test_state.signing_context.session_index)).unwrap();
			}
		);

		// Check that subsystem job issues a request for the availability cores.
		assert_matches!(
			virtual_overseer.recv().await,
			AllMessages::RuntimeApi(
				RuntimeApiMessage::Request(parent, RuntimeApiRequest::AvailabilityCores(tx))
			) if parent == test_state.relay_parent => {
				tx.send(Ok(test_state.availability_cores.clone())).unwrap();
			}
		);
	}

	// Test that a `CandidateBackingMessage::Second` issues validation work
	// and in case validation is successful issues a `StatementDistributionMessage`.
	#[test]
	fn backing_second_works() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov = PoV {
				block_data: BlockData(vec![42, 43, 44]),
			};

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			let pov_hash = pov.hash();
			let candidate = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash,
				head_data: expected_head_data.clone(),
				erasure_root: make_erasure_root(&test_state, pov.clone()),
				..Default::default()
			}.build();

			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate.to_plain(),
				pov.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;


			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromChainState(
						c,
						pov,
						tx,
					)
				) if pov == pov && &c == candidate.descriptor() => {
					tx.send(Ok(
						ValidationResult::Valid(ValidationOutputs {
							head_data: expected_head_data.clone(),
							horizontal_messages: Vec::new(),
							upward_messages: Vec::new(),
							new_validation_code: None,
							processed_downward_messages: 0,
							hrmp_watermark: 0,
						}, test_state.validation_data.persisted),
					)).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::AvailabilityStore(
					AvailabilityStoreMessage::StoreAvailableData(candidate_hash, _, _, _, tx)
				) if candidate_hash == candidate.hash() => {
					tx.send(Ok(())).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						parent_hash,
						signed_statement,
					)
				) if parent_hash == test_state.relay_parent => {
					signed_statement.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[0],
					).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::PoVDistribution(PoVDistributionMessage::DistributePoV(hash, descriptor, pov_received)) => {
					assert_eq!(test_state.relay_parent, hash);
					assert_eq!(candidate.descriptor, descriptor);
					assert_eq!(pov, *pov_received);
				}
			);

			virtual_overseer.send(FromOverseer::Signal(
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::stop_work(test_state.relay_parent)))
			).await;
		});
	}

	// Test that the candidate reaches quorum succesfully.
	#[test]
	fn backing_works() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov = PoV {
				block_data: BlockData(vec![1, 2, 3]),
			};

			let pov_hash = pov.hash();

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			let candidate_a = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash,
				head_data: expected_head_data.clone(),
				erasure_root: make_erasure_root(&test_state, pov.clone()),
				..Default::default()
			}.build();

			let candidate_a_hash = candidate_a.hash();
			let public0 = CryptoStore::sr25519_generate_new(
				&*test_state.keystore,
				ValidatorId::ID,
				Some(&test_state.validators[0].to_seed()),
			).await.expect("Insert key into keystore");
			let public1 = CryptoStore::sr25519_generate_new(
				&*test_state.keystore,
				ValidatorId::ID,
				Some(&test_state.validators[5].to_seed()),
			).await.expect("Insert key into keystore");
			let public2 = CryptoStore::sr25519_generate_new(
				&*test_state.keystore,
				ValidatorId::ID,
				Some(&test_state.validators[2].to_seed()),
			).await.expect("Insert key into keystore");

			let signed_a = SignedFullStatement::sign(
				&test_state.keystore,
				Statement::Seconded(candidate_a.clone()),
				&test_state.signing_context,
				2,
				&public2.into(),
			).await.expect("should be signed");

			let signed_b = SignedFullStatement::sign(
				&test_state.keystore,
				Statement::Valid(candidate_a_hash),
				&test_state.signing_context,
				5,
				&public1.into(),
			).await.expect("should be signed");

			let statement = CandidateBackingMessage::Statement(test_state.relay_parent, signed_a.clone());

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			// Sending a `Statement::Seconded` for our assignment will start
			// validation process. The first thing requested is PoV from the
			// `PoVDistribution`.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::PoVDistribution(
					PoVDistributionMessage::FetchPoV(relay_parent, _, tx)
				) if relay_parent == test_state.relay_parent => {
					tx.send(Arc::new(pov.clone())).unwrap();
				}
			);

			// The next step is the actual request to Validation subsystem
			// to validate the `Seconded` candidate.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromChainState(
						c,
						pov,
						tx,
					)
				) if pov == pov && &c == candidate_a.descriptor() => {
					tx.send(Ok(
						ValidationResult::Valid(ValidationOutputs {
							head_data: expected_head_data.clone(),
							upward_messages: Vec::new(),
							horizontal_messages: Vec::new(),
							new_validation_code: None,
							processed_downward_messages: 0,
							hrmp_watermark: 0,
						}, test_state.validation_data.persisted),
					)).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::AvailabilityStore(
					AvailabilityStoreMessage::StoreAvailableData(candidate_hash, _, _, _, tx)
				) if candidate_hash == candidate_a.hash() => {
					tx.send(Ok(())).unwrap();
				}
			);

			let statement = CandidateBackingMessage::Statement(
				test_state.relay_parent,
				signed_b.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(hash, stmt)
				) => {
					assert_eq!(test_state.relay_parent, hash);
					stmt.check_signature(&test_state.signing_context, &public0.into()).expect("Is signed correctly");
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::Provisioner(
					ProvisionerMessage::ProvisionableData(
						_,
						ProvisionableData::BackedCandidate(BackedCandidate {
							candidate,
							validity_votes,
							validator_indices,
						})
					)
				) if candidate == candidate_a => {
					assert_eq!(validity_votes.len(), 3);

					assert!(validity_votes.contains(
						&ValidityAttestation::Explicit(signed_b.signature().clone())
					));
					assert!(validity_votes.contains(
						&ValidityAttestation::Implicit(signed_a.signature().clone())
					));
					assert_eq!(validator_indices, bitvec::bitvec![Lsb0, u8; 1, 1, 0, 1]);
				}
			);

			virtual_overseer.send(FromOverseer::Signal(
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::stop_work(test_state.relay_parent)))
			).await;
		});
	}

	// Issuing conflicting statements on the same candidate should
	// be a misbehavior.
	#[test]
	fn backing_misbehavior_works() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov = PoV {
				block_data: BlockData(vec![1, 2, 3]),
			};

			let pov_hash = pov.hash();

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			let candidate_a = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash,
				erasure_root: make_erasure_root(&test_state, pov.clone()),
				head_data: expected_head_data.clone(),
				..Default::default()
			}.build();

			let candidate_a_hash = candidate_a.hash();
			let public0 = CryptoStore::sr25519_generate_new(
				&*test_state.keystore,
				ValidatorId::ID, Some(&test_state.validators[0].to_seed())
			).await.expect("Insert key into keystore");
			let public2 = CryptoStore::sr25519_generate_new(
				&*test_state.keystore,
				ValidatorId::ID, Some(&test_state.validators[2].to_seed())
			).await.expect("Insert key into keystore");
			let signed_a = SignedFullStatement::sign(
				&test_state.keystore,
				Statement::Seconded(candidate_a.clone()),
				&test_state.signing_context,
				2,
				&public2.into(),
			).await.expect("should be signed");

			let signed_b = SignedFullStatement::sign(
				&test_state.keystore,
				Statement::Invalid(candidate_a_hash),
				&test_state.signing_context,
				2,
				&public2.into(),
			).await.expect("should be signed");

			let signed_c = SignedFullStatement::sign(
				&test_state.keystore,
				Statement::Invalid(candidate_a_hash),
				&test_state.signing_context,
				0,
				&public0.into(),
			).await.expect("should be signed");

			let statement = CandidateBackingMessage::Statement(test_state.relay_parent, signed_a.clone());

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::PoVDistribution(
					PoVDistributionMessage::FetchPoV(relay_parent, _, tx)
				) if relay_parent == test_state.relay_parent => {
					tx.send(Arc::new(pov.clone())).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromChainState(
						c,
						pov,
						tx,
					)
				) if pov == pov && &c == candidate_a.descriptor() => {
					tx.send(Ok(
						ValidationResult::Valid(ValidationOutputs {
							head_data: expected_head_data.clone(),
							upward_messages: Vec::new(),
							horizontal_messages: Vec::new(),
							new_validation_code: None,
							processed_downward_messages: 0,
							hrmp_watermark: 0,
						}, test_state.validation_data.persisted),
					)).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::AvailabilityStore(
					AvailabilityStoreMessage::StoreAvailableData(candidate_hash, _, _, _, tx)
				) if candidate_hash == candidate_a.hash() => {
						tx.send(Ok(())).unwrap();
					}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						relay_parent,
						signed_statement,
					)
				) if relay_parent == test_state.relay_parent => {
					signed_statement.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[0],
					).unwrap();

					assert_eq!(*signed_statement.payload(), Statement::Valid(candidate_a_hash));
				}
			);

			// This `Invalid` statement contradicts the `Candidate` statement
			// sent at first.
			let statement = CandidateBackingMessage::Statement(test_state.relay_parent, signed_b.clone());

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::Provisioner(
					ProvisionerMessage::ProvisionableData(
						_,
						ProvisionableData::MisbehaviorReport(
							relay_parent,
							MisbehaviorReport::SelfContradiction(_, s1, s2),
						)
					)
				) if relay_parent == test_state.relay_parent => {
					s1.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[s1.validator_index() as usize],
					).unwrap();

					s2.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[s2.validator_index() as usize],
					).unwrap();
				}
			);

			// This `Invalid` statement contradicts the `Valid` statement the subsystem
			// should have issued behind the scenes.
			let statement = CandidateBackingMessage::Statement(test_state.relay_parent, signed_c.clone());

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::Provisioner(
					ProvisionerMessage::ProvisionableData(
						_,
						ProvisionableData::MisbehaviorReport(
							relay_parent,
							MisbehaviorReport::SelfContradiction(_, s1, s2),
						)
					)
				) if relay_parent == test_state.relay_parent => {
					s1.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[s1.validator_index() as usize],
					).unwrap();

					s2.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[s2.validator_index() as usize],
					).unwrap();
				}
			);
		});
	}

	// Test that if we are asked to second an invalid candidate we
	// can still second a valid one afterwards.
	#[test]
	fn backing_dont_second_invalid() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov_block_a = PoV {
				block_data: BlockData(vec![42, 43, 44]),
			};

			let pov_block_b = PoV {
				block_data: BlockData(vec![45, 46, 47]),
			};

			let pov_hash_a = pov_block_a.hash();
			let pov_hash_b = pov_block_b.hash();

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			let candidate_a = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash: pov_hash_a,
				erasure_root: make_erasure_root(&test_state, pov_block_a.clone()),
				..Default::default()
			}.build();

			let candidate_b = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash: pov_hash_b,
				erasure_root: make_erasure_root(&test_state, pov_block_b.clone()),
				head_data: expected_head_data.clone(),
				..Default::default()
			}.build();

			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate_a.to_plain(),
				pov_block_a.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;


			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromChainState(
						c,
						pov,
						tx,
					)
				) if pov == pov && &c == candidate_a.descriptor() => {
					tx.send(Ok(ValidationResult::Invalid(InvalidCandidate::BadReturn))).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateSelection(
					CandidateSelectionMessage::Invalid(parent_hash, c)
				) if parent_hash == test_state.relay_parent && c == candidate_a.to_plain()
			);

			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate_b.to_plain(),
				pov_block_b.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromChainState(
						c,
						pov,
						tx,
					)
				) if pov == pov && &c == candidate_b.descriptor() => {
					tx.send(Ok(
						ValidationResult::Valid(ValidationOutputs {
							head_data: expected_head_data.clone(),
							upward_messages: Vec::new(),
							horizontal_messages: Vec::new(),
							new_validation_code: None,
							processed_downward_messages: 0,
							hrmp_watermark: 0,
						}, test_state.validation_data.persisted),
					)).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::AvailabilityStore(
					AvailabilityStoreMessage::StoreAvailableData(candidate_hash, _, _, _, tx)
				) if candidate_hash == candidate_b.hash() => {
					tx.send(Ok(())).unwrap();
				}
			);

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						parent_hash,
						signed_statement,
					)
				) if parent_hash == test_state.relay_parent => {
					signed_statement.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[0],
					).unwrap();

					assert_eq!(*signed_statement.payload(), Statement::Seconded(candidate_b));
				}
			);

			virtual_overseer.send(FromOverseer::Signal(
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::stop_work(test_state.relay_parent)))
			).await;
		});
	}

	// Test that if we have already issued a statement (in this case `Invalid`) about a
	// candidate we will not be issuing a `Seconded` statement on it.
	#[test]
	fn backing_multiple_statements_work() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov = PoV {
				block_data: BlockData(vec![42, 43, 44]),
			};

			let pov_hash = pov.hash();

			let candidate = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash,
				erasure_root: make_erasure_root(&test_state, pov.clone()),
				..Default::default()
			}.build();

			let candidate_hash = candidate.hash();

			let validator2 = CryptoStore::sr25519_generate_new(
				&*test_state.keystore,
				ValidatorId::ID, Some(&test_state.validators[2].to_seed())
			).await.expect("Insert key into keystore");

			let signed_a = SignedFullStatement::sign(
				&test_state.keystore,
				Statement::Seconded(candidate.clone()),
				&test_state.signing_context,
				2,
				&validator2.into(),
			).await.expect("should be signed");

			// Send in a `Statement` with a candidate.
			let statement = CandidateBackingMessage::Statement(
				test_state.relay_parent,
				signed_a.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			// Subsystem requests PoV and requests validation.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::PoVDistribution(
					PoVDistributionMessage::FetchPoV(relay_parent, _, tx)
				) => {
					assert_eq!(relay_parent, test_state.relay_parent);
					tx.send(Arc::new(pov.clone())).unwrap();
				}
			);


			// Tell subsystem that this candidate is invalid.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromChainState(
						c,
						pov,
						tx,
					)
				) if pov == pov && &c == candidate.descriptor() => {
					tx.send(Ok(ValidationResult::Invalid(InvalidCandidate::BadReturn))).unwrap();
				}
			);

			// The invalid message is shared.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::StatementDistribution(
					StatementDistributionMessage::Share(
						relay_parent,
						signed_statement,
					)
				) => {
					assert_eq!(relay_parent, test_state.relay_parent);
					signed_statement.check_signature(
						&test_state.signing_context,
						&test_state.validator_public[0],
					).unwrap();
					assert_eq!(*signed_statement.payload(), Statement::Invalid(candidate_hash));
				}
			);

			// Ask subsystem to `Second` a candidate that already has a statement issued about.
			// This should emit no actions from subsystem.
			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate.to_plain(),
				pov.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;

			let pov_to_second = PoV {
				block_data: BlockData(vec![3, 2, 1]),
			};

			let pov_hash = pov_to_second.hash();

			let candidate_to_second = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash,
				erasure_root: make_erasure_root(&test_state, pov_to_second.clone()),
				..Default::default()
			}.build();

			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate_to_second.to_plain(),
				pov_to_second.clone(),
			);

			// In order to trigger _some_ actions from subsystem ask it to second another
			// candidate. The only reason to do so is to make sure that no actions were
			// triggered on the prev step.
			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromChainState(
						_,
						pov,
						_,
					)
				) => {
					assert_eq!(&*pov, &pov_to_second);
				}
			);
		});
	}

	// That that if the validation of the candidate has failed this does not stop
	// the work of this subsystem and so it is not fatal to the node.
	#[test]
	fn backing_works_after_failed_validation() {
		let test_state = TestState::default();
		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov = PoV {
				block_data: BlockData(vec![42, 43, 44]),
			};

			let pov_hash = pov.hash();

			let candidate = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash,
				erasure_root: make_erasure_root(&test_state, pov.clone()),
				..Default::default()
			}.build();

			let public2 = CryptoStore::sr25519_generate_new(
				&*test_state.keystore,
				ValidatorId::ID, Some(&test_state.validators[2].to_seed())
			).await.expect("Insert key into keystore");
			let signed_a = SignedFullStatement::sign(
				&test_state.keystore,
				Statement::Seconded(candidate.clone()),
				&test_state.signing_context,
				2,
				&public2.into(),
			).await.expect("should be signed");

			// Send in a `Statement` with a candidate.
			let statement = CandidateBackingMessage::Statement(
				test_state.relay_parent,
				signed_a.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			// Subsystem requests PoV and requests validation.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::PoVDistribution(
					PoVDistributionMessage::FetchPoV(relay_parent, _, tx)
				) => {
					assert_eq!(relay_parent, test_state.relay_parent);
					tx.send(Arc::new(pov.clone())).unwrap();
				}
			);

			// Tell subsystem that this candidate is invalid.
			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateValidation(
					CandidateValidationMessage::ValidateFromChainState(
						c,
						pov,
						tx,
					)
				) if pov == pov && &c == candidate.descriptor() => {
					tx.send(Err(ValidationFailed("Internal test error".into()))).unwrap();
				}
			);

			// Try to get a set of backable candidates to trigger _some_ action in the subsystem
			// and check that it is still alive.
			let (tx, rx) = oneshot::channel();
			let msg = CandidateBackingMessage::GetBackedCandidates(
				test_state.relay_parent,
				tx,
			);

			virtual_overseer.send(FromOverseer::Communication{ msg }).await;
			assert_eq!(rx.await.unwrap().len(), 0);
		});
	}

	// Test that a `CandidateBackingMessage::Second` issues validation work
	// and in case validation is successful issues a `StatementDistributionMessage`.
	#[test]
	fn backing_doesnt_second_wrong_collator() {
		let mut test_state = TestState::default();
		test_state.availability_cores[0] = CoreState::Scheduled(ScheduledCore {
			para_id: ParaId::from(1),
			collator: Some(Sr25519Keyring::Bob.public().into()),
		});

		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov = PoV {
				block_data: BlockData(vec![42, 43, 44]),
			};

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			let pov_hash = pov.hash();
			let candidate = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash,
				head_data: expected_head_data.clone(),
				erasure_root: make_erasure_root(&test_state, pov.clone()),
				..Default::default()
			}.build();

			let second = CandidateBackingMessage::Second(
				test_state.relay_parent,
				candidate.to_plain(),
				pov.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: second }).await;

			assert_matches!(
				virtual_overseer.recv().await,
				AllMessages::CandidateSelection(
					CandidateSelectionMessage::Invalid(parent, c)
				) if parent == test_state.relay_parent && c == candidate.to_plain() => {
				}
			);

			virtual_overseer.send(FromOverseer::Signal(
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::stop_work(test_state.relay_parent)))
			).await;
		});
	}

	#[test]
	fn validation_work_ignores_wrong_collator() {
		let mut test_state = TestState::default();
		test_state.availability_cores[0] = CoreState::Scheduled(ScheduledCore {
			para_id: ParaId::from(1),
			collator: Some(Sr25519Keyring::Bob.public().into()),
		});

		test_harness(test_state.keystore.clone(), |test_harness| async move {
			let TestHarness { mut virtual_overseer } = test_harness;

			test_startup(&mut virtual_overseer, &test_state).await;

			let pov = PoV {
				block_data: BlockData(vec![1, 2, 3]),
			};

			let pov_hash = pov.hash();

			let expected_head_data = test_state.head_data.get(&test_state.chain_ids[0]).unwrap();

			let candidate_a = TestCandidateBuilder {
				para_id: test_state.chain_ids[0],
				relay_parent: test_state.relay_parent,
				pov_hash,
				head_data: expected_head_data.clone(),
				erasure_root: make_erasure_root(&test_state, pov.clone()),
				..Default::default()
			}.build();

			let public2 = CryptoStore::sr25519_generate_new(
				&*test_state.keystore,
				ValidatorId::ID, Some(&test_state.validators[2].to_seed())
			).await.expect("Insert key into keystore");
			let seconding = SignedFullStatement::sign(
				&test_state.keystore,
				Statement::Seconded(candidate_a.clone()),
				&test_state.signing_context,
				2,
				&public2.into(),
			).await.expect("should be signed");

			let statement = CandidateBackingMessage::Statement(
				test_state.relay_parent,
				seconding.clone(),
			);

			virtual_overseer.send(FromOverseer::Communication{ msg: statement }).await;

			// The statement will be ignored because it has the wrong collator.
			virtual_overseer.send(FromOverseer::Signal(
				OverseerSignal::ActiveLeaves(ActiveLeavesUpdate::stop_work(test_state.relay_parent)))
			).await;
		});
	}
}
