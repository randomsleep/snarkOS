// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{fmt_id, spawn_blocking, LedgerService};
use snarkvm::{
    console::account::PrivateKey,
    ledger::{
        block::{Block, Transaction},
        coinbase::{CoinbaseVerifyingKey, ProverSolution, PuzzleCommitment},
        committee::Committee,
        narwhal::{BatchCertificate, Data, Subdag, Transmission, TransmissionID},
        store::ConsensusStorage,
        Ledger,
    },
    prelude::{bail, Field, Network, Result},
};

use indexmap::IndexMap;
use std::{
    fmt,
    ops::Range,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

/// A core ledger service.
pub struct CoreLedgerService<N: Network, C: ConsensusStorage<N>> {
    ledger: Ledger<N, C>,
    coinbase_verifying_key: Arc<CoinbaseVerifyingKey<N>>,
    shutdown: Arc<AtomicBool>,
}

impl<N: Network, C: ConsensusStorage<N>> CoreLedgerService<N, C> {
    /// Initializes a new core ledger service.
    pub fn new(ledger: Ledger<N, C>, shutdown: Arc<AtomicBool>) -> Self {
        let coinbase_verifying_key = Arc::new(ledger.coinbase_puzzle().coinbase_verifying_key().clone());
        Self { ledger, coinbase_verifying_key, shutdown }
    }
}

impl<N: Network, C: ConsensusStorage<N>> fmt::Debug for CoreLedgerService<N, C> {
    /// Implements a custom `fmt::Debug` for `CoreLedgerService`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreLedgerService").field("current_committee", &self.current_committee()).finish()
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> LedgerService<N> for CoreLedgerService<N, C> {
    fn generate_bond_transaction(&self, amount: u64, private_key: PrivateKey<N>) -> Result<Transaction<N>> {
        use snarkvm::{
            console::{
                program::{Identifier, Literal, ProgramID, Value},
                types::U64,
            },
            prelude::Address,
        };
        use std::str::FromStr;

        let locator_bond = (ProgramID::from_str("credits.aleo")?, Identifier::from_str("bond_public")?);
        let to_address = Literal::Address(Address::try_from(private_key).unwrap());
        let inputs = [Value::from(to_address), Value::from(Literal::U64(U64::new(amount)))];
        // Execute the transaction.
        let transaction = self.ledger.vm().execute(
            &private_key,
            locator_bond,
            inputs.into_iter(),
            None,
            0, // set priority to 0 to make it easier to simulate
            None,
            &mut rand::thread_rng(),
        );

        transaction
    }

    /// Returns the latest round in the ledger.
    fn latest_round(&self) -> u64 {
        self.ledger.latest_round()
    }

    /// Returns the latest block height in the ledger.
    fn latest_block_height(&self) -> u32 {
        self.ledger.latest_height()
    }

    /// Returns the latest block in the ledger.
    fn latest_block(&self) -> Block<N> {
        self.ledger.latest_block()
    }

    /// Returns `true` if the given block height exists in the ledger.
    fn contains_block_height(&self, height: u32) -> bool {
        self.ledger.contains_block_height(height).unwrap_or(false)
    }

    /// Returns the block height for the given block hash, if it exists.
    fn get_block_height(&self, hash: &N::BlockHash) -> Result<u32> {
        self.ledger.get_height(hash)
    }

    /// Returns the block hash for the given block height, if it exists.
    fn get_block_hash(&self, height: u32) -> Result<N::BlockHash> {
        self.ledger.get_hash(height)
    }

    /// Returns the block for the given block height.
    fn get_block(&self, height: u32) -> Result<Block<N>> {
        self.ledger.get_block(height)
    }

    /// Returns the blocks in the given block range.
    /// The range is inclusive of the start and exclusive of the end.
    fn get_blocks(&self, heights: Range<u32>) -> Result<Vec<Block<N>>> {
        self.ledger.get_blocks(heights)
    }

    /// Returns the solution for the given solution ID.
    fn get_solution(&self, solution_id: &PuzzleCommitment<N>) -> Result<ProverSolution<N>> {
        self.ledger.get_solution(solution_id)
    }

    /// Returns the unconfirmed transaction for the given transaction ID.
    fn get_unconfirmed_transaction(&self, transaction_id: N::TransactionID) -> Result<Transaction<N>> {
        self.ledger.get_unconfirmed_transaction(&transaction_id)
    }

    /// Returns the batch certificate for the given batch certificate ID.
    fn get_batch_certificate(&self, certificate_id: &Field<N>) -> Result<BatchCertificate<N>> {
        match self.ledger.get_batch_certificate(certificate_id) {
            Ok(Some(certificate)) => Ok(certificate),
            Ok(None) => bail!("No batch certificate found for certificate ID {certificate_id} in the ledger"),
            Err(error) => Err(error),
        }
    }

    /// Returns the current committee.
    fn current_committee(&self) -> Result<Committee<N>> {
        self.ledger.latest_committee()
    }

    /// Returns the committee for the given round.
    /// If the given round is in the future, then the current committee is returned.
    fn get_committee_for_round(&self, round: u64) -> Result<Committee<N>> {
        match self.ledger.get_committee_for_round(round)? {
            // Return the committee if it exists.
            Some(committee) => Ok(committee),
            // Return the current committee if the round is in the future.
            None => {
                // Retrieve the current committee.
                let current_committee = self.current_committee()?;
                // Return the current committee if the round is in the future.
                match current_committee.starting_round() <= round {
                    true => Ok(current_committee),
                    false => bail!("No committee found for round {round} in the ledger"),
                }
            }
        }
    }

    /// Returns the previous committee for the given round.
    /// If the previous round is in the future, then the current committee is returned.
    fn get_previous_committee_for_round(&self, round: u64) -> Result<Committee<N>> {
        // Get the round number for the previous committee. Note, we subtract 2 from odd rounds,
        // because committees are updated in even rounds.
        let previous_round = match round % 2 == 0 {
            true => round.saturating_sub(1),
            false => round.saturating_sub(2),
        };

        // Retrieve the committee for the previous round.
        self.get_committee_for_round(previous_round)
    }

    /// Returns `true` if the ledger contains the given certificate ID in block history.
    fn contains_certificate(&self, certificate_id: &Field<N>) -> Result<bool> {
        self.ledger.contains_certificate(certificate_id)
    }

    /// Returns `true` if the transmission exists in the ledger.
    fn contains_transmission(&self, transmission_id: &TransmissionID<N>) -> Result<bool> {
        match transmission_id {
            TransmissionID::Ratification => Ok(false),
            TransmissionID::Solution(puzzle_commitment) => self.ledger.contains_puzzle_commitment(puzzle_commitment),
            TransmissionID::Transaction(transaction_id) => self.ledger.contains_transaction_id(transaction_id),
        }
    }

    /// Ensures the given transmission ID matches the given transmission.
    fn ensure_transmission_id_matches(
        &self,
        transmission_id: TransmissionID<N>,
        transmission: &mut Transmission<N>,
    ) -> Result<()> {
        match (transmission_id, transmission) {
            (TransmissionID::Ratification, Transmission::Ratification) => {}
            (TransmissionID::Transaction(expected_transaction_id), Transmission::Transaction(transaction_data)) => {
                match transaction_data.clone().deserialize_blocking() {
                    Ok(transaction) => {
                        if transaction.id() != expected_transaction_id {
                            bail!(
                                "Received mismatching transaction ID  - expected {}, found {}",
                                fmt_id(expected_transaction_id),
                                fmt_id(transaction.id()),
                            );
                        }

                        // Update the transmission with the deserialized transaction.
                        *transaction_data = Data::Object(transaction);
                    }
                    Err(err) => {
                        bail!("Failed to deserialize transaction: {err}");
                    }
                }
            }
            (TransmissionID::Solution(expected_commitment), Transmission::Solution(solution_data)) => {
                match solution_data.clone().deserialize_blocking() {
                    Ok(solution) => {
                        if solution.commitment() != expected_commitment {
                            bail!(
                                "Received mismatching solution ID - expected {}, found {}",
                                fmt_id(expected_commitment),
                                fmt_id(solution.commitment()),
                            );
                        }

                        // Update the transmission with the deserialized solution.
                        *solution_data = Data::Object(solution);
                    }
                    Err(err) => {
                        bail!("Failed to deserialize solution: {err}");
                    }
                }
            }
            _ => {
                bail!("Mismatching `(transmission_id, transmission)` pair");
            }
        }

        Ok(())
    }

    /// Checks the given solution is well-formed.
    async fn check_solution_basic(
        &self,
        puzzle_commitment: PuzzleCommitment<N>,
        solution: Data<ProverSolution<N>>,
    ) -> Result<()> {
        // Deserialize the solution.
        let solution = spawn_blocking!(solution.deserialize_blocking())?;
        // Ensure the puzzle commitment matches in the solution.
        if puzzle_commitment != solution.commitment() {
            bail!("Invalid solution - expected {puzzle_commitment}, found {}", solution.commitment());
        }

        // Retrieve the coinbase verifying key.
        let coinbase_verifying_key = self.coinbase_verifying_key.clone();
        // Compute the current epoch challenge.
        let epoch_challenge = self.ledger.latest_epoch_challenge()?;
        // Retrieve the current proof target.
        let proof_target = self.ledger.latest_proof_target();

        // Ensure that the prover solution is valid for the given epoch.
        if !spawn_blocking!(solution.verify(&coinbase_verifying_key, &epoch_challenge, proof_target))? {
            bail!("Invalid prover solution '{puzzle_commitment}' for the current epoch.");
        }
        Ok(())
    }

    /// Checks the given transaction is well-formed and unique.
    async fn check_transaction_basic(
        &self,
        transaction_id: N::TransactionID,
        transaction: Data<Transaction<N>>,
    ) -> Result<()> {
        // Deserialize the transaction.
        let transaction = spawn_blocking!(transaction.deserialize_blocking())?;
        // Ensure the transaction ID matches in the transaction.
        if transaction_id != transaction.id() {
            bail!("Invalid transaction - expected {transaction_id}, found {}", transaction.id());
        }
        // Check if the transmission is a fee transaction.
        if transaction.is_fee() {
            bail!("Invalid transaction - 'Transaction::fee' type is not valid at this stage ({})", transaction.id());
        }
        // Check the transaction is well-formed.
        let ledger = self.ledger.clone();
        spawn_blocking!(ledger.check_transaction_basic(&transaction, None, &mut rand::thread_rng()))
    }

    /// Checks the given block is valid next block.
    fn check_next_block(&self, block: &Block<N>) -> Result<()> {
        self.ledger.check_next_block(block, &mut rand::thread_rng())
    }

    /// Returns a candidate for the next block in the ledger, using a committed subdag and its transmissions.
    #[cfg(feature = "ledger-write")]
    fn prepare_advance_to_next_quorum_block(
        &self,
        subdag: Subdag<N>,
        transmissions: IndexMap<TransmissionID<N>, Transmission<N>>,
    ) -> Result<Block<N>> {
        self.ledger.prepare_advance_to_next_quorum_block(subdag, transmissions)
    }

    /// Adds the given block as the next block in the ledger.
    #[cfg(feature = "ledger-write")]
    fn advance_to_next_block(&self, block: &Block<N>) -> Result<()> {
        // If the Ctrl-C handler registered the signal, then skip advancing to the next block.
        if self.shutdown.load(Ordering::Relaxed) {
            bail!("Skipping advancing to block {} - The node is shutting down", block.height());
        }
        // Advance to the next block.
        self.ledger.advance_to_next_block(block)?;
        tracing::info!("\n\nAdvanced to block {} at round {} - {}\n", block.height(), block.round(), block.hash());
        Ok(())
    }
}
