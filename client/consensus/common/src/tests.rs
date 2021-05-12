// Copyright 2019-2021 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

use crate::*;

use codec::Encode;
use cumulus_test_client::{
	runtime::{Block, Header},
	Client, InitBlockBuilder, TestClientBuilder, TestClientBuilderExt,
};
use futures::{channel::mpsc, executor::block_on, select, FutureExt, Stream, StreamExt};
use futures_timer::Delay;
use polkadot_overseer::{AllSubsystems, HeadSupportsParachains, Overseer, OverseerHandler};
use polkadot_primitives::v1::{
	Block as PBlock, CommittedCandidateReceipt, Id as ParaId, SessionIndex,
};
use sc_client_api::UsageProvider;
use sp_blockchain::{Error as ClientError, Result as ClientResult};
use sp_consensus::{BlockImport, BlockImportParams, BlockOrigin, ForkChoiceStrategy};
use sp_core::testing::TaskExecutor;
use sp_runtime::generic::BlockId;
use std::{
	sync::{Arc, Mutex},
	time::Duration,
};

struct AlwaysSupportsParachains;
impl HeadSupportsParachains for AlwaysSupportsParachains {
	fn head_supports_parachains(&self, _head: &PHash) -> bool {
		true
	}
}

fn create_overseer() -> (
	Overseer<TaskExecutor, AlwaysSupportsParachains>,
	OverseerHandler,
) {
	let spawner = TaskExecutor::new();
	let all_subsystems = AllSubsystems::<()>::dummy();
	Overseer::new(
		vec![],
		all_subsystems,
		None,
		AlwaysSupportsParachains,
		spawner,
	)
	.unwrap()
}

struct RelaychainInner {
	new_best_heads: Option<mpsc::UnboundedReceiver<Header>>,
	finalized_heads: Option<mpsc::UnboundedReceiver<Header>>,
	new_best_heads_sender: mpsc::UnboundedSender<Header>,
	finalized_heads_sender: mpsc::UnboundedSender<Header>,
}

impl RelaychainInner {
	fn new() -> Self {
		let (new_best_heads_sender, new_best_heads) = mpsc::unbounded();
		let (finalized_heads_sender, finalized_heads) = mpsc::unbounded();

		Self {
			new_best_heads_sender,
			finalized_heads_sender,
			new_best_heads: Some(new_best_heads),
			finalized_heads: Some(finalized_heads),
		}
	}
}

#[derive(Clone)]
struct Relaychain {
	inner: Arc<Mutex<RelaychainInner>>,
}

impl Relaychain {
	fn new() -> Self {
		Self {
			inner: Arc::new(Mutex::new(RelaychainInner::new())),
		}
	}
}

impl crate::parachain_consensus::RelaychainClient for Relaychain {
	type Error = ClientError;

	type HeadStream = Box<dyn Stream<Item = Vec<u8>> + Send + Unpin>;

	type PendingCandidateStream =
		Box<dyn Stream<Item = (CommittedCandidateReceipt, SessionIndex)> + Send + Unpin>;

	fn new_best_heads(&self, _: ParaId) -> Self::HeadStream {
		let stream = self
			.inner
			.lock()
			.unwrap()
			.new_best_heads
			.take()
			.expect("Should only be called once");

		Box::new(stream.map(|v| v.encode()))
	}

	fn finalized_heads(&self, _: ParaId) -> Self::HeadStream {
		let stream = self
			.inner
			.lock()
			.unwrap()
			.finalized_heads
			.take()
			.expect("Should only be called once");

		Box::new(stream.map(|v| v.encode()))
	}

	fn parachain_head_at(&self, _: &BlockId<PBlock>, _: ParaId) -> ClientResult<Option<Vec<u8>>> {
		unimplemented!("Not required for tests")
	}

	fn pending_candidates(&self, _: ParaId) -> Self::PendingCandidateStream {
		Box::new(futures::stream::pending())
	}
}

fn build_and_import_block(mut client: Arc<Client>) -> Block {
	let builder = client.init_block_builder(None, Default::default());

	let block = builder.build().unwrap().block;
	let (header, body) = block.clone().deconstruct();

	let mut block_import_params = BlockImportParams::new(BlockOrigin::Own, header);
	block_import_params.fork_choice = Some(ForkChoiceStrategy::Custom(false));
	block_import_params.body = Some(body);

	block_on(client.import_block(block_import_params, Default::default())).unwrap();
	assert_eq!(0, client.chain_info().best_number);

	block
}

#[test]
fn follow_new_best_works() {
	sp_tracing::try_init_simple();

	let (_overseer, overseer_handler) = create_overseer();
	let client = Arc::new(TestClientBuilder::default().build());

	let block = build_and_import_block(client.clone());
	let relay_chain = Relaychain::new();
	let new_best_heads_sender = relay_chain
		.inner
		.lock()
		.unwrap()
		.new_best_heads_sender
		.clone();

	let consensus = run_parachain_consensus(
		100.into(),
		client.clone(),
		relay_chain,
		Arc::new(|_, _| {}),
		None,
	);

	let work = async move {
		new_best_heads_sender
			.unbounded_send(block.header().clone())
			.unwrap();
		loop {
			Delay::new(Duration::from_millis(100)).await;
			if block.hash() == client.usage_info().chain.best_hash {
				break;
			}
		}
	};

	block_on(async move {
		futures::pin_mut!(consensus);
		futures::pin_mut!(work);

		select! {
			r = consensus.fuse() => panic!("Consensus should not end: {:?}", r),
			_ = work.fuse() => {},
		}
	});
}

#[test]
fn follow_finalized_works() {
	sp_tracing::try_init_simple();

	let (_overseer, overseer_handler) = create_overseer();
	let client = Arc::new(TestClientBuilder::default().build());

	let block = build_and_import_block(client.clone());
	let relay_chain = Relaychain::new();
	let finalized_sender = relay_chain
		.inner
		.lock()
		.unwrap()
		.finalized_heads_sender
		.clone();

	let consensus = run_parachain_consensus(
		100.into(),
		client.clone(),
		relay_chain,
		Arc::new(|_, _| {}),
		None,
	);

	let work = async move {
		finalized_sender
			.unbounded_send(block.header().clone())
			.unwrap();
		loop {
			Delay::new(Duration::from_millis(100)).await;
			if block.hash() == client.usage_info().chain.finalized_hash {
				break;
			}
		}
	};

	block_on(async move {
		futures::pin_mut!(consensus);
		futures::pin_mut!(work);

		select! {
			r = consensus.fuse() => panic!("Consensus should not end: {:?}", r),
			_ = work.fuse() => {},
		}
	});
}

#[test]
fn follow_finalized_does_not_stop_on_unknown_block() {
	sp_tracing::try_init_simple();

	let (_overseer, overseer_handler) = create_overseer();
	let client = Arc::new(TestClientBuilder::default().build());

	let block = build_and_import_block(client.clone());

	let unknown_block = {
		let block_builder =
			client.init_block_builder_at(&BlockId::Hash(block.hash()), None, Default::default());
		block_builder.build().unwrap().block
	};

	let relay_chain = Relaychain::new();
	let finalized_sender = relay_chain
		.inner
		.lock()
		.unwrap()
		.finalized_heads_sender
		.clone();

	let consensus = run_parachain_consensus(
		100.into(),
		client.clone(),
		relay_chain,
		Arc::new(|_, _| {}),
		None,
	);

	let work = async move {
		for _ in 0..3usize {
			finalized_sender
				.unbounded_send(unknown_block.header().clone())
				.unwrap();

			Delay::new(Duration::from_millis(100)).await;
		}

		finalized_sender
			.unbounded_send(block.header().clone())
			.unwrap();
		loop {
			Delay::new(Duration::from_millis(100)).await;
			if block.hash() == client.usage_info().chain.finalized_hash {
				break;
			}
		}
	};

	block_on(async move {
		futures::pin_mut!(consensus);
		futures::pin_mut!(work);

		select! {
			r = consensus.fuse() => panic!("Consensus should not end: {:?}", r),
			_ = work.fuse() => {},
		}
	});
}

// It can happen that we first import a relay chain block, while not yet having the parachain
// block imported that would be set to the best block. We need to make sure to import this
// block as new best block in the moment it is imported.
#[test]
fn follow_new_best_sets_best_after_it_is_imported() {
	sp_tracing::try_init_simple();

	let (_overseer, overseer_handler) = create_overseer();
	let mut client = Arc::new(TestClientBuilder::default().build());

	let block = build_and_import_block(client.clone());

	let unknown_block = {
		let block_builder =
			client.init_block_builder_at(&BlockId::Hash(block.hash()), None, Default::default());
		block_builder.build().unwrap().block
	};

	let relay_chain = Relaychain::new();
	let new_best_heads_sender = relay_chain
		.inner
		.lock()
		.unwrap()
		.new_best_heads_sender
		.clone();

	let consensus = run_parachain_consensus(
		100.into(),
		client.clone(),
		relay_chain,
		Arc::new(|_, _| {}),
		None,
	);

	let work = async move {
		new_best_heads_sender
			.unbounded_send(block.header().clone())
			.unwrap();

		loop {
			Delay::new(Duration::from_millis(100)).await;
			if block.hash() == client.usage_info().chain.best_hash {
				break;
			}
		}

		// Announce the unknown block
		new_best_heads_sender
			.unbounded_send(unknown_block.header().clone())
			.unwrap();

		// Do some iterations. As this is a local task executor, only one task can run at a time.
		// Meaning that it should already have processed the unknown block.
		for _ in 0..3usize {
			Delay::new(Duration::from_millis(100)).await;
		}

		let (header, body) = unknown_block.clone().deconstruct();

		let mut block_import_params = BlockImportParams::new(BlockOrigin::Own, header);
		block_import_params.fork_choice = Some(ForkChoiceStrategy::Custom(false));
		block_import_params.body = Some(body);

		// Now import the unkown block to make it "known"
		client
			.import_block(block_import_params, Default::default())
			.await
			.unwrap();

		loop {
			Delay::new(Duration::from_millis(100)).await;
			if unknown_block.hash() == client.usage_info().chain.best_hash {
				break;
			}
		}
	};

	block_on(async move {
		futures::pin_mut!(consensus);
		futures::pin_mut!(work);

		select! {
			r = consensus.fuse() => panic!("Consensus should not end: {:?}", r),
			_ = work.fuse() => {},
		}
	});
}
