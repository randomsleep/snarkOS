#[cfg(debug_assertions)]
use snarkos_algorithms::snark::PreparedVerifyingKey;
use snarkos_dpc::{
    ledger::Ledger,
    payment_dpc::{
        instantiated::*,
        payment_circuit::*,
        predicate::PrivatePredInput,
        record_payload::PaymentRecordPayload,
        LocalData,
        PaymentDPCComponents,
        DPC,
    },
    DPCScheme,
    Record,
};
use snarkos_models::algorithms::{CommitmentScheme, CRH, SNARK};
use snarkos_utilities::{bytes::ToBytes, rand::UniformRand, to_bytes};

use rand::SeedableRng;
use rand_xorshift::XorShiftRng;

#[test]
fn plain_payment_dpc_integration_test() {
    let mut rng = XorShiftRng::seed_from_u64(23472342u64);
    // Generate parameters for the ledger, commitment schemes, CRH, and the
    // "always-accept" predicate.
    let ledger_parameters = MerkleTreeIdealLedger::setup(&mut rng).expect("Ledger setup failed");

    let parameters = <InstantiatedDPC as DPCScheme<MerkleTreeIdealLedger>>::setup(&ledger_parameters, &mut rng)
        .expect("DPC setup failed");

    #[cfg(debug_assertions)]
    let pred_nizk_pvk: PreparedVerifyingKey<_> = parameters.pred_nizk_pp.vk.clone().into();
    // Generate metadata and an address for a dummy initial, or "genesis", record.
    let genesis_metadata = [1u8; 32];
    let genesis_address = DPC::create_address_helper(&parameters.comm_and_crh_pp, &genesis_metadata, &mut rng).unwrap();

    let genesis_sn_nonce = SnNonceCRH::hash(&parameters.comm_and_crh_pp.sn_nonce_crh_pp, &[34u8; 1]).unwrap();
    let genesis_pred_vk_bytes = to_bytes![
        PredVkCRH::hash(
            &parameters.comm_and_crh_pp.pred_vk_crh_pp,
            &to_bytes![parameters.pred_nizk_pp.vk].unwrap()
        )
        .unwrap()
    ]
    .unwrap();

    let genesis_record = DPC::generate_record(
        &parameters.comm_and_crh_pp,
        &genesis_sn_nonce,
        &genesis_address.public_key,
        true, // The inital record should be dummy
        &PaymentRecordPayload::default(),
        &Predicate::new(genesis_pred_vk_bytes.clone()),
        &Predicate::new(genesis_pred_vk_bytes.clone()),
        &mut rng,
    )
    .unwrap();

    // Generate serial number for the genesis record.
    let genesis_sn = DPC::generate_sn(&genesis_record, &genesis_address.secret_key).unwrap();
    let genesis_memo = [1u8; 32];

    // Use genesis record, serial number, and memo to initialize the ledger.
    let mut ledger = MerkleTreeIdealLedger::new(
        ledger_parameters,
        genesis_record.commitment(),
        genesis_sn.clone(),
        genesis_memo,
    );

    // Generate dummy input records having as address the genesis address.
    let old_asks = vec![genesis_address.secret_key.clone(); NUM_INPUT_RECORDS];
    let mut old_records = vec![];
    for i in 0..NUM_INPUT_RECORDS {
        let old_sn_nonce =
            SnNonceCRH::hash(&parameters.comm_and_crh_pp.sn_nonce_crh_pp, &[64u8 + (i as u8); 1]).unwrap();
        let old_record = DPC::generate_record(
            &parameters.comm_and_crh_pp,
            &old_sn_nonce,
            &genesis_address.public_key,
            true, // The input record is dummy
            &PaymentRecordPayload::default(),
            &Predicate::new(genesis_pred_vk_bytes.clone()),
            &Predicate::new(genesis_pred_vk_bytes.clone()),
            &mut rng,
        )
        .unwrap();
        old_records.push(old_record);
    }

    // Construct new records.

    // Create an address for an actual new record.
    let new_metadata = [2u8; 32];
    let new_address = DPC::create_address_helper(&parameters.comm_and_crh_pp, &new_metadata, &mut rng).unwrap();

    // Create a payload.
    let new_payload = PaymentRecordPayload::default();
    // Set the new records' predicate to be the "always-accept" predicate.
    let new_predicate = Predicate::new(genesis_pred_vk_bytes.clone());

    let new_apks = vec![new_address.public_key.clone(); NUM_OUTPUT_RECORDS];
    let new_payloads = vec![new_payload.clone(); NUM_OUTPUT_RECORDS];
    let new_birth_predicates = vec![new_predicate.clone(); NUM_OUTPUT_RECORDS];
    let new_death_predicates = vec![new_predicate.clone(); NUM_OUTPUT_RECORDS];
    let new_dummy_flags = vec![false; NUM_OUTPUT_RECORDS];

    let auxiliary = [3u8; 32];
    let memo = [4u8; 32];

    let old_death_vk_and_proof_generator = |local_data: &LocalData<Components>| {
        let mut rng = XorShiftRng::seed_from_u64(23472342u64);
        let mut old_proof_and_vk = vec![];
        for i in 0..NUM_INPUT_RECORDS {
            let input_value = local_data.old_records[i].payload().balance;

            // Generate value commitment randomness
            let value_commitment_randomness =
                <<Components as PaymentDPCComponents>::ValueComm as CommitmentScheme>::Randomness::rand(&mut rng);

            // Generate the value commitment
            let value_commitment = local_data
                .comm_and_crh_pp
                .value_comm_pp
                .commit(&input_value.to_le_bytes(), &value_commitment_randomness)
                .unwrap();

            // Instantiate death predicate circuit
            let death_predicate_circuit = PaymentCircuit::new(
                &local_data.comm_and_crh_pp,
                &local_data.local_data_comm,
                &value_commitment_randomness,
                &value_commitment,
                i as u8,
                input_value,
            );

            // Generate the predicate proof
            let proof = PredicateNIZK::prove(&parameters.pred_nizk_pp.pk, death_predicate_circuit, &mut rng)
                .expect("Proving should work");
            #[cfg(debug_assertions)]
            {
                let pred_pub_input: PaymentPredicateLocalData<Components> = PaymentPredicateLocalData {
                    local_data_comm_pp: local_data.comm_and_crh_pp.local_data_comm_pp.parameters().clone(),
                    local_data_comm: local_data.local_data_comm.clone(),
                    value_comm_pp: local_data.comm_and_crh_pp.value_comm_pp.parameters().clone(),
                    value_comm_randomness: value_commitment_randomness.clone(),
                    value_commitment: value_commitment.clone(),
                    position: i as u8,
                };
                assert!(PredicateNIZK::verify(&pred_nizk_pvk, &pred_pub_input, &proof).expect("Proof should verify"));
            }

            let private_input: PrivatePredInput<Components> = PrivatePredInput {
                vk: parameters.pred_nizk_pp.vk.clone(),
                proof,
                value_commitment,
                value_commitment_randomness,
            };
            old_proof_and_vk.push(private_input);
        }
        old_proof_and_vk
    };
    let new_birth_vk_and_proof_generator = |local_data: &LocalData<Components>| {
        let mut rng = XorShiftRng::seed_from_u64(23472342u64);
        let mut new_proof_and_vk = vec![];
        for j in 0..NUM_OUTPUT_RECORDS {
            let output_value = local_data.new_records[j].payload().balance;

            // Generate value commitment randomness
            let value_commitment_randomness =
                <<Components as PaymentDPCComponents>::ValueComm as CommitmentScheme>::Randomness::rand(&mut rng);

            // Generate the value commitment
            let value_commitment = local_data
                .comm_and_crh_pp
                .value_comm_pp
                .commit(&output_value.to_le_bytes(), &value_commitment_randomness)
                .unwrap();

            // Instantiate birth predicate circuit
            let birth_predicate_circuit = PaymentCircuit::new(
                &local_data.comm_and_crh_pp,
                &local_data.local_data_comm,
                &value_commitment_randomness,
                &value_commitment,
                j as u8,
                output_value,
            );

            // Generate the predicate proof
            let proof = PredicateNIZK::prove(&parameters.pred_nizk_pp.pk, birth_predicate_circuit, &mut rng)
                .expect("Proving should work");
            #[cfg(debug_assertions)]
            {
                let pred_pub_input: PaymentPredicateLocalData<Components> = PaymentPredicateLocalData {
                    local_data_comm_pp: local_data.comm_and_crh_pp.local_data_comm_pp.parameters().clone(),
                    local_data_comm: local_data.local_data_comm.clone(),
                    value_comm_pp: local_data.comm_and_crh_pp.value_comm_pp.parameters().clone(),
                    value_comm_randomness: value_commitment_randomness.clone(),
                    value_commitment: value_commitment.clone(),
                    position: j as u8,
                };
                assert!(PredicateNIZK::verify(&pred_nizk_pvk, &pred_pub_input, &proof).expect("Proof should verify"));
            }
            let private_input: PrivatePredInput<Components> = PrivatePredInput {
                vk: parameters.pred_nizk_pp.vk.clone(),
                proof,
                value_commitment,
                value_commitment_randomness,
            };
            new_proof_and_vk.push(private_input);
        }
        new_proof_and_vk
    };
    let (_new_records, transaction) = InstantiatedDPC::execute(
        &parameters,
        &old_records,
        &old_asks,
        &old_death_vk_and_proof_generator,
        &new_apks,
        &new_dummy_flags,
        &new_payloads,
        &new_birth_predicates,
        &new_death_predicates,
        &new_birth_vk_and_proof_generator,
        &auxiliary,
        &memo,
        &ledger,
        &mut rng,
    )
    .unwrap();

    assert!(InstantiatedDPC::verify(&parameters, &transaction, &ledger).unwrap());

    ledger.push(transaction).unwrap();
    assert_eq!(ledger.len(), 1);
}
