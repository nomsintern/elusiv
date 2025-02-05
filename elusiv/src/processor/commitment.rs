use super::utils::{close_account, open_pda_account_with_offset};
use crate::bytes::usize_as_u32_safe;
use crate::commitment::{
    commitment_hash_computation_instructions, commitments_per_batch,
    compute_base_commitment_hash_partial, compute_commitment_hash_partial,
    BaseCommitmentHashComputation, MAX_HT_COMMITMENTS,
};
use crate::error::ElusivError;
use crate::fields::{fr_to_u256_le, is_element_scalar_field, u256_to_big_uint, u256_to_fr_skip_mr};
use crate::macros::{guard, pda_account, BorshSerDeSized};
use crate::processor::utils::{
    transfer_lamports_from_pda_checked, transfer_token, transfer_token_from_pda,
    transfer_with_system_program, verify_program_token_account,
};
use crate::state::commitment::{
    BaseCommitmentBufferAccount, BaseCommitmentHashingAccount, CommitmentHashingAccount,
};
use crate::state::governor::FeeCollectorAccount;
use crate::state::storage::{StorageAccount, MT_COMMITMENT_COUNT};
use crate::state::{
    fee::FeeAccount,
    governor::GovernorAccount,
    queue::{CommitmentQueue, CommitmentQueueAccount, Queue, RingQueue},
};
use crate::token::{Token, TokenPrice};
use crate::types::{RawU256, U256};
use ark_bn254::Fr;
use ark_ff::BigInteger256;
use borsh::{BorshDeserialize, BorshSerialize};
use elusiv_computation::PartialComputation;
use solana_program::{account_info::AccountInfo, entrypoint::ProgramResult};

#[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized, PartialEq, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct BaseCommitmentHashRequest {
    pub base_commitment: RawU256,
    pub commitment_index: u32,
    pub amount: u64,
    pub token_id: u16,
    pub commitment: RawU256, // only there in case we require duplicate checking (not atm)
    pub fee_version: u32,

    /// The minimum allowed batching rate (since the fee is precomputed with the concrete batching rate)
    pub min_batching_rate: u32,
}

#[derive(
    BorshDeserialize, BorshSerialize, BorshSerDeSized, PartialEq, Copy, Clone, Debug, Default,
)]
pub struct CommitmentHashRequest {
    pub commitment: U256,
    pub fee_version: u32,
    pub min_batching_rate: u32,
}

/// poseidon(0, 0)
const ZERO_BASE_COMMITMENT: Fr = Fr::new(BigInteger256::new([
    3162363550698150530,
    9486080942857866267,
    15374008727889305678,
    621823773387469172,
]));

/// poseidon(poseidon(0, 0), 0) in mr-form
pub const ZERO_COMMITMENT: U256 = [
    29, 226, 44, 239, 152, 247, 24, 127, 109, 7, 41, 61, 125, 1, 193, 123, 69, 104, 37, 230, 178,
    56, 26, 51, 102, 9, 129, 182, 119, 238, 153, 4,
];

/// poseidon(poseidon(0, 0), 0)
pub const ZERO_COMMITMENT_RAW: U256 = [
    106, 77, 49, 231, 137, 82, 142, 103, 122, 195, 234, 157, 189, 191, 2, 42, 174, 41, 59, 182, 21,
    225, 230, 119, 13, 86, 164, 94, 87, 82, 83, 23,
];

/// Stores a base commitment hash and takes the funds from the sender
///
/// # Notes
///
/// Initializes the computation: `commitment = poseidon(base_commitment, amount + token_id * 2^64)` (https://github.com/elusiv-privacy/circuits/blob/master/circuits/commitment.circom).
///
/// Signatures of both `sender` and `fee_payer` are required.
///
/// `sender`: wants to store the commitment (pays amount and fee).
///
/// `fee_payer`:
///     - opens a [`BaseCommitmentHashingAccount`] for the computation,
///     - performs the hash computation,
///     - swaps fee from token into lamports (for tx compensation of the commitment hash).
#[allow(clippy::too_many_arguments)]
pub fn store_base_commitment<'a>(
    sender: &AccountInfo<'a>,
    sender_account: &AccountInfo<'a>,
    fee_payer: &AccountInfo<'a>,
    fee_payer_account: &AccountInfo<'a>,
    pool: &AccountInfo<'a>,
    pool_account: &AccountInfo<'a>,
    fee_collector: &AccountInfo<'a>,
    fee_collector_account: &AccountInfo<'a>,

    sol_usd_price_account: &AccountInfo,
    token_usd_price_account: &AccountInfo,

    governor: &GovernorAccount,
    hashing_account: &AccountInfo<'a>,
    base_commitment_buffer: &mut BaseCommitmentBufferAccount,
    token_program: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,

    hash_account_index: u32,
    hash_account_bump: u8,
    request: BaseCommitmentHashRequest,
) -> ProgramResult {
    let token_id = request.token_id;
    let amount = Token::new_checked(token_id, request.amount)?;
    let price = TokenPrice::new(sol_usd_price_account, token_usd_price_account, token_id)?;

    guard!(
        is_element_scalar_field(u256_to_big_uint(&request.base_commitment.skip_mr())),
        ElusivError::NonScalarValue
    );
    guard!(
        is_element_scalar_field(u256_to_big_uint(&request.commitment.skip_mr())),
        ElusivError::NonScalarValue
    );

    // TODO: verify commitment-index in the next SDK version

    // Zero-commitment cannot be inserted by user
    guard!(
        u256_to_fr_skip_mr(&request.base_commitment.reduce()) != ZERO_BASE_COMMITMENT,
        ElusivError::InvalidInstructionData
    );

    guard!(
        request.fee_version == governor.get_fee_version(),
        ElusivError::InvalidFeeVersion
    );
    guard!(
        request.min_batching_rate == governor.get_commitment_batching_rate(),
        ElusivError::InvalidBatchingRate
    );

    let fee = governor.get_program_fee();
    let subvention = fee
        .base_commitment_subvention
        .into_token(&price, token_id)?;
    let computation_fee = (fee.base_commitment_hash_computation_fee()
        + fee.commitment_hash_computation_fee(request.min_batching_rate))?;
    let computation_fee_token = computation_fee.into_token(&price, token_id)?;
    let network_fee = Token::new(
        token_id,
        fee.base_commitment_network_fee.calc(amount.amount()),
    );

    verify_program_token_account(pool, pool_account, token_id)?;
    verify_program_token_account(fee_collector, fee_collector_account, token_id)?;

    // `sender` transfers `computation_fee_token` - `subvention` to `fee_payer` (token)
    transfer_token(
        sender,
        sender_account,
        fee_payer_account,
        token_program,
        (computation_fee_token - subvention)?,
    )?;

    // `fee_payer` transfers `computation_fee` to `pool` (lamports)
    transfer_with_system_program(fee_payer, pool, system_program, computation_fee.0)?;

    // `sender` transfers `network_fee` to `fee_collector` (token)
    transfer_token(
        sender,
        sender_account,
        fee_collector_account,
        token_program,
        network_fee,
    )?;

    // `sender` transfers `amount` to `pool` (token)
    transfer_token(sender, sender_account, pool_account, token_program, amount)?;

    // `fee_payer` rents `hashing_account`
    open_pda_account_with_offset::<BaseCommitmentHashingAccount>(
        &crate::id(),
        fee_payer,
        hashing_account,
        hash_account_index,
        Some(hash_account_bump),
    )?;

    // `fee_collector` transfers `subvention` to `fee_payer` (token)
    transfer_token_from_pda::<FeeCollectorAccount>(
        fee_collector,
        fee_collector_account,
        fee_payer_account,
        token_program,
        subvention,
        None,
        None,
    )?;

    // Buffer duplicate check and insertion
    base_commitment_buffer.try_insert(&request.base_commitment.skip_mr())?;

    // `hashing_account` setup
    pda_account!(
        mut hashing_account,
        BaseCommitmentHashingAccount,
        hashing_account
    );
    hashing_account.setup(request, fee_payer.key.to_bytes())
}

// TODO: add functionality for a Warden to compute other uncomputed base-commitments (initiated by other Wardens)
pub fn compute_base_commitment_hash(
    hashing_account: &mut BaseCommitmentHashingAccount,

    _hash_account_index: u32,
) -> ProgramResult {
    guard!(
        hashing_account.get_is_active(),
        ElusivError::ComputationIsNotYetStarted
    );
    compute_base_commitment_hash_partial(hashing_account)
}

#[allow(clippy::too_many_arguments)]
pub fn finalize_base_commitment_hash<'a>(
    original_fee_payer: &AccountInfo<'a>,
    pool: &AccountInfo<'a>,
    fee: &FeeAccount,
    hashing_account_info: &AccountInfo<'a>,
    commitment_hash_queue: &mut CommitmentQueueAccount,

    _hash_account_index: u32,
    fee_version: u32,
) -> ProgramResult {
    pda_account!(
        mut hashing_account,
        BaseCommitmentHashingAccount,
        hashing_account_info
    );
    guard!(
        hashing_account.get_fee_version() == fee_version,
        ElusivError::InvalidFeeVersion
    );
    guard!(
        hashing_account.get_is_active(),
        ElusivError::ComputationIsNotYetStarted
    );
    guard!(
        hashing_account.get_fee_payer() == original_fee_payer.key.to_bytes(),
        ElusivError::InvalidAccount
    );
    guard!(
        (hashing_account.get_instruction() as usize) == BaseCommitmentHashComputation::IX_COUNT,
        ElusivError::ComputationIsNotYetFinished
    );

    // `pool` transfers `base_commitment_hash_fee` to `original_fee_payer` (lamports)
    transfer_lamports_from_pda_checked(
        pool,
        original_fee_payer,
        fee.get_program_fee()
            .base_commitment_hash_computation_fee()
            .0,
    )?;

    let commitment = hashing_account.get_state().result();
    let mut commitment_queue = CommitmentQueue::new(commitment_hash_queue);
    commitment_queue.enqueue(CommitmentHashRequest {
        commitment: fr_to_u256_le(&commitment),
        fee_version,
        min_batching_rate: hashing_account.get_min_batching_rate(),
    })?;

    // Close hashing account
    hashing_account.set_is_active(&false);
    close_account(original_fee_payer, hashing_account_info)
}

/// Places the hash siblings into the hashing account
pub fn init_commitment_hash_setup(
    hashing_account: &mut CommitmentHashingAccount,
    storage_account: &StorageAccount,

    insertion_can_fail: bool,
) -> ProgramResult {
    match init_commitment_hash_setup_inner(hashing_account, storage_account) {
        Ok(()) => Ok(()),
        Err(e) => {
            if insertion_can_fail {
                solana_program::msg!("Instruction failed: {:?}", e);
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

fn init_commitment_hash_setup_inner(
    hashing_account: &mut CommitmentHashingAccount,
    storage_account: &StorageAccount,
) -> ProgramResult {
    guard!(
        !hashing_account.get_is_active(),
        ElusivError::ComputationIsNotYetFinished
    );

    let ordering = storage_account.get_next_commitment_ptr();
    let siblings = storage_account.get_mt_opening(ordering as usize)?;

    hashing_account.setup(ordering, &siblings)
}

/// Places the next batch from the commitment queue in the [`CommitmentHashingAccount`]
pub fn init_commitment_hash(
    queue: &mut CommitmentQueueAccount,
    hashing_account: &mut CommitmentHashingAccount,

    insertion_can_fail: bool,
) -> ProgramResult {
    match init_commitment_hash_inner(queue, hashing_account) {
        Ok(()) => Ok(()),
        Err(e) => {
            if insertion_can_fail {
                solana_program::msg!("Instruction failed: {:?}", e);
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

fn init_commitment_hash_inner(
    queue: &mut CommitmentQueueAccount,
    hashing_account: &mut CommitmentHashingAccount,
) -> ProgramResult {
    guard!(
        !hashing_account.get_is_active(),
        ElusivError::ComputationIsNotYetFinished
    );
    guard!(
        hashing_account.get_setup(),
        ElusivError::ComputationIsNotYetFinished
    );

    let mut queue = CommitmentQueue::new(queue);
    let (batch, batching_rate) = queue.next_batch()?;
    queue.remove(usize_as_u32_safe(batch.len()))?;

    // The fee/batch-upgrader logic has to guarantee that there are no lower fees in a batch
    let fee_version = batch.first().unwrap().fee_version;

    // Check for room for the commitment batch
    guard!(
        hashing_account.get_ordering() as usize + batch.len() <= MT_COMMITMENT_COUNT,
        ElusivError::NoRoomForCommitment
    );

    let mut commitments = [[0; 32]; MAX_HT_COMMITMENTS];
    for i in 0..batch.len() {
        commitments[i] = batch[i].commitment;
    }

    hashing_account.reset(batching_rate, fee_version, &commitments)
}

pub fn compute_commitment_hash<'a>(
    fee_payer: &AccountInfo<'a>,
    fee: &FeeAccount,
    pool: &AccountInfo<'a>,
    hashing_account: &mut CommitmentHashingAccount,

    fee_version: u32,
    _nonce: u32,
) -> ProgramResult {
    guard!(
        hashing_account.get_is_active(),
        ElusivError::ComputationIsNotYetStarted
    );
    guard!(
        hashing_account.get_fee_version() == fee_version,
        ElusivError::InvalidFeeVersion
    );

    compute_commitment_hash_partial(hashing_account)?;

    transfer_lamports_from_pda_checked(
        pool,
        fee_payer,
        fee.get_program_fee().hash_tx_compensation().0,
    )
}

/// Requires `batching_rate + 1` calls
pub fn finalize_commitment_hash(
    hashing_account: &mut CommitmentHashingAccount,
    storage_account: &mut StorageAccount,
) -> ProgramResult {
    guard!(
        hashing_account.get_is_active(),
        ElusivError::ComputationIsNotYetStarted
    );

    let finalization_ix = hashing_account.get_finalization_ix();
    let batching_rate = hashing_account.get_batching_rate();
    guard!(
        finalization_ix <= batching_rate,
        ElusivError::ComputationIsAlreadyFinished
    );

    let instruction = hashing_account.get_instruction();
    let instructions =
        commitment_hash_computation_instructions(hashing_account.get_batching_rate());
    guard!(
        (instruction as usize) >= instructions.len(),
        ElusivError::ComputationIsAlreadyFinished
    );

    guard!(
        storage_account.get_next_commitment_ptr() as usize + commitments_per_batch(batching_rate)
            <= MT_COMMITMENT_COUNT,
        ElusivError::NoRoomForCommitment
    );

    hashing_account.update_mt(storage_account, finalization_ix);
    hashing_account.set_finalization_ix(&(finalization_ix + 1));
    if finalization_ix == batching_rate {
        hashing_account.set_is_active(&false);
        hashing_account.set_setup(&false);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitment::poseidon_hash::full_poseidon2_hash;
    use crate::fields::{
        big_uint_to_u256, fr_to_u256_le_repr, u256_from_str_skip_mr, SCALAR_MODULUS_RAW,
    };
    use crate::macros::{
        account_info, parent_account, program_token_account_info, pyth_price_account_info,
        test_account_info, test_pda_account_info, zero_program_account,
    };
    use crate::state::governor::PoolAccount;
    use crate::state::program_account::{PDAAccount, SizedAccount};
    use crate::state::storage::{EMPTY_TREE, MT_HEIGHT};
    use crate::token::{lamports_token, usdc_token, LAMPORTS_TOKEN_ID, USDC_TOKEN_ID};
    use ark_ff::Zero;
    use assert_matches::assert_matches;
    use elusiv_types::tokens::Price;
    use solana_program::native_token::LAMPORTS_PER_SOL;
    use solana_program::pubkey::Pubkey;
    use solana_program::system_program;
    use std::str::FromStr;

    #[test]
    fn test_zero_commitment() {
        assert_eq!(
            fr_to_u256_le(
                &Fr::from_str(
                    "10550412122474489085186760340904980499891544584677836768300371073631951867242"
                )
                .unwrap()
            ),
            ZERO_COMMITMENT
        );

        assert_eq!(
            full_poseidon2_hash(full_poseidon2_hash(Fr::zero(), Fr::zero()), Fr::zero()),
            u256_to_fr_skip_mr(&ZERO_COMMITMENT)
        );

        assert_eq!(RawU256::new(ZERO_COMMITMENT_RAW).reduce(), ZERO_COMMITMENT);

        assert_eq!(
            full_poseidon2_hash(Fr::zero(), Fr::zero()),
            ZERO_BASE_COMMITMENT
        );
    }

    #[test]
    fn test_store_base_commitment_lamports() {
        zero_program_account!(mut governor, GovernorAccount);
        zero_program_account!(mut buffer, BaseCommitmentBufferAccount);
        test_account_info!(sender, 0);
        test_account_info!(fee_payer, 0);
        test_account_info!(pool, 0);
        test_account_info!(fee_collector, 0);
        test_account_info!(any, 0);
        account_info!(sys, system_program::id(), vec![]);
        account_info!(spl, spl_token::id(), vec![]);
        let (hasing_account_pubkey, bump) = BaseCommitmentHashingAccount::find(Some(0));
        account_info!(
            hashing_acc,
            hasing_account_pubkey,
            vec![0; BaseCommitmentHashingAccount::SIZE]
        );

        governor.set_commitment_batching_rate(&4);
        governor.set_fee_version(&1);

        let request = BaseCommitmentHashRequest {
            base_commitment: RawU256::new(u256_from_str_skip_mr("1")),
            commitment_index: 123,
            amount: LAMPORTS_PER_SOL,
            token_id: LAMPORTS_TOKEN_ID,
            commitment: RawU256::new(u256_from_str_skip_mr("1")),
            fee_version: 1,
            min_batching_rate: 4,
        };

        // Amount too low
        let mut requests = vec![request.clone()];
        requests.last_mut().unwrap().amount = lamports_token().min - 1;

        // Amount too high
        requests.push(request.clone());
        requests.last_mut().unwrap().amount = lamports_token().max + 1;

        // Non-scalar base_commitment
        requests.push(request.clone());
        requests.last_mut().unwrap().base_commitment =
            RawU256::new(big_uint_to_u256(&SCALAR_MODULUS_RAW));

        // Non-scalar commitment
        requests.push(request.clone());
        requests.last_mut().unwrap().commitment =
            RawU256::new(big_uint_to_u256(&SCALAR_MODULUS_RAW));

        // Zero-commitment
        requests.push(request.clone());
        requests.last_mut().unwrap().base_commitment =
            RawU256::new(fr_to_u256_le_repr(&ZERO_BASE_COMMITMENT));

        // Mismatched fee_version
        requests.push(request.clone());
        requests.last_mut().unwrap().fee_version = 0;

        // Invalid min_batching_rate
        requests.push(request.clone());
        requests.last_mut().unwrap().min_batching_rate = 0;

        for request in requests {
            assert_matches!(
                store_base_commitment(
                    &sender,
                    &sender,
                    &fee_payer,
                    &fee_payer,
                    &pool,
                    &pool,
                    &fee_collector,
                    &fee_collector,
                    &any,
                    &any,
                    &governor,
                    &hashing_acc,
                    &mut buffer,
                    &sys,
                    &sys,
                    0,
                    bump,
                    request
                ),
                Err(_)
            );
        }

        // Invalid pool_account
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender,
                &fee_payer,
                &fee_payer,
                &pool,
                &any,
                &fee_collector,
                &fee_collector,
                &any,
                &any,
                &governor,
                &hashing_acc,
                &mut buffer,
                &sys,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Invalid fee_collector_account
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender,
                &fee_payer,
                &fee_payer,
                &pool,
                &pool,
                &fee_collector,
                &any,
                &any,
                &any,
                &governor,
                &hashing_acc,
                &mut buffer,
                &sys,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Invalid token_program
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender,
                &fee_payer,
                &fee_payer,
                &pool,
                &pool,
                &fee_collector,
                &pool,
                &any,
                &any,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Mismatch between PDA and offset
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender,
                &fee_payer,
                &fee_payer,
                &pool,
                &pool,
                &fee_collector,
                &pool,
                &any,
                &any,
                &governor,
                &hashing_acc,
                &mut buffer,
                &sys,
                &sys,
                1,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Invalid bump
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender,
                &fee_payer,
                &fee_payer,
                &pool,
                &pool,
                &fee_collector,
                &fee_collector,
                &any,
                &any,
                &governor,
                &hashing_acc,
                &mut buffer,
                &sys,
                &sys,
                0,
                0,
                request.clone()
            ),
            Err(_)
        );

        assert_matches!(
            store_base_commitment(
                &sender,
                &sender,
                &fee_payer,
                &fee_payer,
                &pool,
                &pool,
                &fee_collector,
                &fee_collector,
                &any,
                &any,
                &governor,
                &hashing_acc,
                &mut buffer,
                &sys,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Ok(())
        );

        // Immediate uplicate insertion will fail
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender,
                &fee_payer,
                &fee_payer,
                &pool,
                &pool,
                &fee_collector,
                &fee_collector,
                &any,
                &any,
                &governor,
                &hashing_acc,
                &mut buffer,
                &sys,
                &sys,
                0,
                bump,
                request
            ),
            Err(_)
        );
    }

    #[test]
    fn test_store_base_commitment_token() {
        zero_program_account!(governor, GovernorAccount);
        zero_program_account!(mut buffer, BaseCommitmentBufferAccount);
        test_account_info!(sender);
        test_account_info!(fee_payer);
        test_account_info!(sender_token, 0, spl_token::id());
        test_account_info!(fee_payer_token, 0, spl_token::id());
        test_pda_account_info!(pool, PoolAccount);
        test_pda_account_info!(fee_c, FeeCollectorAccount);
        program_token_account_info!(pool_token, PoolAccount, USDC_TOKEN_ID);
        program_token_account_info!(fee_c_token, FeeCollectorAccount, USDC_TOKEN_ID);
        account_info!(sys, system_program::id(), vec![]);
        account_info!(spl, spl_token::id(), vec![]);
        let (hasing_account_pubkey, bump) = BaseCommitmentHashingAccount::find(Some(0));
        account_info!(
            hashing_acc,
            hasing_account_pubkey,
            vec![0; BaseCommitmentHashingAccount::SIZE]
        );

        let sol_usd = Price {
            price: 39,
            conf: 1,
            expo: 0,
        };
        let usdc_usd = Price {
            price: 1,
            conf: 1,
            expo: 0,
        };
        pyth_price_account_info!(sol, LAMPORTS_TOKEN_ID, sol_usd);
        pyth_price_account_info!(usdc, USDC_TOKEN_ID, usdc_usd);

        let request = BaseCommitmentHashRequest {
            base_commitment: RawU256::new(u256_from_str_skip_mr("1")),
            commitment_index: 123,
            amount: 1_000_000,
            token_id: USDC_TOKEN_ID,
            commitment: RawU256::new(u256_from_str_skip_mr("1")),
            fee_version: 0,
            min_batching_rate: 0,
        };

        // Amount too low
        let mut requests = vec![request.clone()];
        requests.last_mut().unwrap().amount = usdc_token().min - 1;

        // Amount too high
        requests.push(request.clone());
        requests.last_mut().unwrap().amount = usdc_token().max + 1;

        for request in requests {
            assert_matches!(
                store_base_commitment(
                    &sender,
                    &sender_token,
                    &fee_payer,
                    &fee_payer_token,
                    &pool,
                    &pool_token,
                    &fee_c,
                    &fee_c_token,
                    &sol,
                    &usdc,
                    &governor,
                    &hashing_acc,
                    &mut buffer,
                    &spl,
                    &sys,
                    0,
                    bump,
                    request
                ),
                Err(_)
            );
        }

        // Invalid pool_account
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender_token,
                &fee_payer,
                &fee_payer_token,
                &pool,
                &fee_c_token,
                &fee_c,
                &fee_c_token,
                &sol,
                &usdc,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Invalid fee_collector_account
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender_token,
                &fee_payer,
                &fee_payer_token,
                &pool,
                &pool_token,
                &fee_c,
                &pool_token,
                &sol,
                &usdc,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Invalid token_program
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender_token,
                &fee_payer,
                &fee_payer_token,
                &pool,
                &pool_token,
                &fee_c,
                &fee_c_token,
                &sol,
                &usdc,
                &governor,
                &hashing_acc,
                &mut buffer,
                &sys,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Mismatch between PDA and offset
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender_token,
                &fee_payer,
                &fee_payer_token,
                &pool,
                &pool_token,
                &fee_c,
                &fee_c_token,
                &sol,
                &usdc,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                1,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Invalid sender_account
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender,
                &fee_payer,
                &fee_payer_token,
                &pool,
                &pool_token,
                &fee_c,
                &fee_c_token,
                &sol,
                &usdc,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Invalid fee_collector_account
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender_token,
                &fee_payer,
                &fee_payer,
                &pool,
                &pool_token,
                &fee_c,
                &fee_c_token,
                &sol,
                &usdc,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Invalid sol_usd_price_account
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender_token,
                &fee_payer,
                &fee_payer_token,
                &pool,
                &pool_token,
                &fee_c,
                &fee_c_token,
                &usdc,
                &usdc,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        // Invalid token_usd_price_account
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender_token,
                &fee_payer,
                &fee_payer_token,
                &pool,
                &pool_token,
                &fee_c,
                &fee_c_token,
                &sol,
                &sol,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Err(_)
        );

        assert_matches!(
            store_base_commitment(
                &sender,
                &sender_token,
                &fee_payer,
                &fee_payer_token,
                &pool,
                &pool_token,
                &fee_c,
                &fee_c_token,
                &sol,
                &usdc,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                0,
                bump,
                request.clone()
            ),
            Ok(())
        );

        // Immediate uplicate insertion will fail
        assert_matches!(
            store_base_commitment(
                &sender,
                &sender_token,
                &fee_payer,
                &fee_payer_token,
                &pool,
                &pool_token,
                &fee_c,
                &fee_c_token,
                &sol,
                &usdc,
                &governor,
                &hashing_acc,
                &mut buffer,
                &spl,
                &sys,
                0,
                bump,
                request
            ),
            Err(_)
        );
    }

    #[test]
    fn test_compute_base_commitment_hash() {
        zero_program_account!(mut hashing_account, BaseCommitmentHashingAccount);

        // Inactive
        assert_matches!(
            compute_base_commitment_hash(&mut hashing_account, 0),
            Err(_)
        );

        hashing_account.set_is_active(&true);

        for _ in 0..BaseCommitmentHashComputation::IX_COUNT {
            assert_matches!(
                compute_base_commitment_hash(&mut hashing_account, 0),
                Ok(())
            );
        }

        // Additional computations will fail
        assert_matches!(
            compute_base_commitment_hash(&mut hashing_account, 0),
            Err(_)
        );
        assert_eq!(
            hashing_account.get_state().result(),
            Fr::from_str(
                "14744269619966411208579211824598458697587494354926760081771325075741142829156"
            )
            .unwrap()
        );
    }

    #[test]
    fn test_finalize_base_commitment_hash() -> ProgramResult {
        account_info!(fee_payer, Pubkey::new_unique(), vec![0]);
        account_info!(
            h_account,
            BaseCommitmentHashingAccount::find(Some(0)).0,
            vec![0; BaseCommitmentHashingAccount::SIZE]
        );
        zero_program_account!(mut q, CommitmentQueueAccount);
        zero_program_account!(fee, FeeAccount);
        test_account_info!(pool, 0);

        // Inactive hashing account
        {
            pda_account!(mut h, BaseCommitmentHashingAccount, h_account);
            h.set_instruction(&(BaseCommitmentHashComputation::IX_COUNT as u32));
            h.set_fee_payer(&fee_payer.key.to_bytes());
        }
        assert_matches!(
            finalize_base_commitment_hash(&fee_payer, &pool, &fee, &h_account, &mut q, 0, 0),
            Err(_)
        );

        // Invalid original fee payer
        {
            pda_account!(mut h, BaseCommitmentHashingAccount, h_account);
            h.set_is_active(&true);
            h.set_fee_payer(&[0; 32]);
        }
        assert_matches!(
            finalize_base_commitment_hash(&fee_payer, &pool, &fee, &h_account, &mut q, 0, 0),
            Err(_)
        );

        // Computation not finished
        {
            pda_account!(mut h, BaseCommitmentHashingAccount, h_account);
            h.set_instruction(&0);
            h.set_fee_payer(&fee_payer.key.to_bytes());
        }
        assert_matches!(
            finalize_base_commitment_hash(&fee_payer, &pool, &fee, &h_account, &mut q, 0, 0),
            Err(_)
        );

        // Invalid fee version
        assert_matches!(
            finalize_base_commitment_hash(&fee_payer, &pool, &fee, &h_account, &mut q, 0, 1),
            Err(_)
        );

        // Commitment queue is full
        {
            pda_account!(mut h, BaseCommitmentHashingAccount, h_account);
            h.set_instruction(&(BaseCommitmentHashComputation::IX_COUNT as u32));

            let mut q = CommitmentQueue::new(&mut q);
            for _ in 0..CommitmentQueue::CAPACITY {
                q.enqueue(CommitmentHashRequest {
                    commitment: [0; 32],
                    min_batching_rate: 0,
                    fee_version: 0,
                })
                .unwrap();
            }
        }
        assert_matches!(
            finalize_base_commitment_hash(&fee_payer, &pool, &fee, &h_account, &mut q, 0, 0),
            Err(_)
        );

        zero_program_account!(mut q, CommitmentQueueAccount);
        assert_matches!(
            finalize_base_commitment_hash(&fee_payer, &pool, &fee, &h_account, &mut q, 0, 0),
            Ok(())
        );
        Ok(())
    }

    #[test]
    fn test_init_commitment_hash_empty_queue() {
        parent_account!(storage_account, StorageAccount);
        zero_program_account!(mut queue, CommitmentQueueAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);

        init_commitment_hash_setup(&mut hashing_account, &storage_account, false).unwrap();
        assert_matches!(
            init_commitment_hash(&mut queue, &mut hashing_account, false),
            Err(_)
        );
    }

    #[test]
    fn test_init_commitment_hash_active_computation() {
        zero_program_account!(mut queue, CommitmentQueueAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);

        let mut q = CommitmentQueue::new(&mut queue);
        q.enqueue(CommitmentHashRequest {
            commitment: [0; 32],
            min_batching_rate: 0,
            fee_version: 0,
        })
        .unwrap();

        hashing_account.set_is_active(&true);
        hashing_account.set_setup(&true);
        assert_matches!(
            init_commitment_hash(&mut queue, &mut hashing_account, false),
            Err(_)
        );
    }

    #[test]
    fn test_init_commitment_hash_full_storage() {
        parent_account!(mut storage_account, StorageAccount);
        zero_program_account!(mut queue, CommitmentQueueAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);

        let mut q = CommitmentQueue::new(&mut queue);
        q.enqueue(CommitmentHashRequest {
            commitment: [0; 32],
            min_batching_rate: 0,
            fee_version: 0,
        })
        .unwrap();

        storage_account.set_next_commitment_ptr(&(MT_COMMITMENT_COUNT as u32));
        init_commitment_hash_setup(&mut hashing_account, &storage_account, false).unwrap();
        assert_matches!(
            init_commitment_hash(&mut queue, &mut hashing_account, false),
            Err(_)
        );
    }

    #[test]
    fn test_init_commitment_hash_incomplete_batch() {
        parent_account!(storage_account, StorageAccount);
        zero_program_account!(mut queue, CommitmentQueueAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);

        let mut q = CommitmentQueue::new(&mut queue);
        q.enqueue(CommitmentHashRequest {
            commitment: [0; 32],
            min_batching_rate: 1,
            fee_version: 0,
        })
        .unwrap();

        init_commitment_hash_setup(&mut hashing_account, &storage_account, false).unwrap();
        assert_matches!(
            init_commitment_hash(&mut queue, &mut hashing_account, false),
            Err(_)
        );
    }

    #[test]
    fn test_init_commitment_hash_batch_too_big() {
        parent_account!(mut storage_account, StorageAccount);
        zero_program_account!(mut queue, CommitmentQueueAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);

        let mut q = CommitmentQueue::new(&mut queue);
        q.enqueue(CommitmentHashRequest {
            commitment: [0; 32],
            min_batching_rate: 1,
            fee_version: 0,
        })
        .unwrap();

        storage_account.set_next_commitment_ptr(&(MT_COMMITMENT_COUNT as u32 - 1));
        init_commitment_hash_setup(&mut hashing_account, &storage_account, false).unwrap();
        assert_matches!(
            init_commitment_hash(&mut queue, &mut hashing_account, false),
            Err(_)
        );
    }

    #[test]
    #[allow(clippy::needless_range_loop)]
    fn test_init_commitment_hash_valid() {
        parent_account!(storage_account, StorageAccount);
        zero_program_account!(mut queue, CommitmentQueueAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);

        let mut q = CommitmentQueue::new(&mut queue);
        q.enqueue(CommitmentHashRequest {
            commitment: [1; 32],
            min_batching_rate: 2,
            fee_version: 0,
        })
        .unwrap();
        q.enqueue(CommitmentHashRequest {
            commitment: [2; 32],
            min_batching_rate: 0,
            fee_version: 0,
        })
        .unwrap();
        q.enqueue(CommitmentHashRequest {
            commitment: [3; 32],
            min_batching_rate: 0,
            fee_version: 0,
        })
        .unwrap();
        q.enqueue(CommitmentHashRequest {
            commitment: [4; 32],
            min_batching_rate: 0,
            fee_version: 0,
        })
        .unwrap();

        init_commitment_hash_setup(&mut hashing_account, &storage_account, false).unwrap();
        init_commitment_hash(&mut queue, &mut hashing_account, false).unwrap();

        assert_eq!(hashing_account.get_batching_rate(), 2);

        // Check correct siblings
        for i in 0..MT_HEIGHT as usize {
            assert_eq!(hashing_account.get_siblings(i), EMPTY_TREE[i]);
        }

        // Check correct commitments
        for i in 0..4 {
            assert_eq!(hashing_account.get_hash_tree(i), [i as u8 + 1; 32]);
        }
    }

    #[test]
    fn test_init_commitment_hash_setup_insertion_can_fail() {
        parent_account!(storage_account, StorageAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);
        hashing_account.set_is_active(&true);
        assert_matches!(
            init_commitment_hash_setup(&mut hashing_account, &storage_account, false),
            Err(_)
        );
        assert_matches!(
            init_commitment_hash_setup(&mut hashing_account, &storage_account, true),
            Ok(())
        );
    }

    #[test]
    fn test_init_commitment_hash_insertion_can_fail() {
        zero_program_account!(mut queue, CommitmentQueueAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);
        assert_matches!(
            init_commitment_hash(&mut queue, &mut hashing_account, false),
            Err(_)
        );
        assert_matches!(
            init_commitment_hash(&mut queue, &mut hashing_account, true),
            Ok(())
        );
    }

    #[test]
    fn test_compute_commitment_hash() {
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);
        zero_program_account!(fee, FeeAccount);
        test_account_info!(pool, 0);
        test_account_info!(fee_payer, 0);

        // Inactive account
        assert_matches!(
            compute_commitment_hash(&fee_payer, &fee, &pool, &mut hashing_account, 0, 0),
            Err(_)
        );

        // Invalid fee_version
        hashing_account.set_is_active(&true);
        assert_matches!(
            compute_commitment_hash(&fee_payer, &fee, &pool, &mut hashing_account, 1, 0),
            Err(_)
        );

        compute_commitment_hash(&fee_payer, &fee, &pool, &mut hashing_account, 0, 0).unwrap();
    }

    #[test]
    fn test_finalize_commitment_hash() {
        parent_account!(mut storage_account, StorageAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);

        // Computation not finished
        hashing_account.set_is_active(&true);
        hashing_account.set_instruction(&0);
        assert_matches!(
            finalize_commitment_hash(&mut hashing_account, &mut storage_account),
            Err(_)
        );

        // Hashing account inactive
        hashing_account.set_is_active(&false);
        hashing_account
            .set_instruction(&(commitment_hash_computation_instructions(0).len() as u32));
        assert_matches!(
            finalize_commitment_hash(&mut hashing_account, &mut storage_account),
            Err(_)
        );

        // Storage account is full
        hashing_account.set_is_active(&true);
        storage_account.set_next_commitment_ptr(&(MT_COMMITMENT_COUNT as u32));
        assert_matches!(
            finalize_commitment_hash(&mut hashing_account, &mut storage_account),
            Err(_)
        );

        storage_account.set_next_commitment_ptr(&0);
        finalize_commitment_hash(&mut hashing_account, &mut storage_account).unwrap();
    }

    #[test]
    fn test_finalize_commitment_hash_valid() {
        parent_account!(mut storage_account, StorageAccount);
        zero_program_account!(mut hashing_account, CommitmentHashingAccount);

        let batching_rate = 4;
        let commitment_count = commitments_per_batch(batching_rate);
        hashing_account.set_is_active(&true);
        hashing_account.set_batching_rate(&batching_rate);
        hashing_account.set_instruction(
            &(commitment_hash_computation_instructions(batching_rate).len() as u32),
        );

        for level_inv in 0..=MT_HEIGHT {
            let level = MT_HEIGHT - level_inv;
            let level_size = commitment_count >> level;
            for index in 0..level_size {
                assert_eq!(
                    storage_account.get_node(index, level as usize).unwrap(),
                    EMPTY_TREE[level_inv as usize]
                );
            }
        }

        for _ in 0..=batching_rate {
            finalize_commitment_hash(&mut hashing_account, &mut storage_account).unwrap();
        }

        assert!(!hashing_account.get_is_active());
        assert!(!hashing_account.get_setup());
        assert_eq!(
            storage_account.get_next_commitment_ptr(),
            commitment_count as u32
        );

        // Check that MT is updated
        for level_inv in 0..=MT_HEIGHT {
            let level_size = commitment_count >> level_inv;
            let level = MT_HEIGHT - level_inv;
            for index in 0..level_size {
                assert_ne!(
                    storage_account.get_node(index, level as usize).unwrap(),
                    EMPTY_TREE[level_inv as usize]
                );
            }
        }
    }
}
