use std::net::Ipv4Addr;
use borsh::{BorshDeserialize, BorshSerialize};
use elusiv_utils::{open_pda_account_with_offset, pda_account, guard, open_pda_account};
use solana_program::{
    pubkey::Pubkey,
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    program_error::ProgramError,
};
use elusiv_types::{accounts::PDAAccountData, BorshSerDeSized, ProgramAccount};
use crate::{macros::{elusiv_account, BorshSerDeSized}, error::ElusivWardenNetworkError};

/// A unique ID publicly identifying a single Warden
pub type ElusivWardenID = u32;

/// The [`ElusivWardensAccount`] assigns each new Warden it's [`ElusivWardenID`]
#[elusiv_account(eager_type: true)]
pub struct WardensAccount {
    pda_data: PDAAccountData,

    pub next_warden_id: ElusivWardenID,
    pub full_network_configured: bool,
}

impl<'a> WardensAccount<'a> {
    fn inc_next_warden_id(&mut self) -> ProgramResult {
        let next_id = self.get_next_warden_id();

        self.set_next_warden_id(
            &next_id
                .checked_add(1)
                .ok_or_else(|| ProgramError::from(ElusivWardenNetworkError::WardenRegistrationError))?
        );

        Ok(())
    }

    pub fn add_basic_warden<'b>(
        &mut self,
        warden: &AccountInfo<'b>,
        basic_warden: ElusivBasicWarden,
        warden_account: &AccountInfo<'b>,
        warden_map_account: &AccountInfo<'b>,
    ) -> ProgramResult {
        let warden_id = self.get_next_warden_id();
        self.inc_next_warden_id()?;

        open_pda_account_with_offset::<BasicWardenAccount>(
            &crate::id(),
            warden,
            warden_account,
            warden_id,
        )?;

        pda_account!(mut warden_account, BasicWardenAccount, warden_account);
        warden_account.set_warden(&basic_warden);

        // `warden_map_account` is used to store the `warden_id` and prevent duplicate registrations
        open_pda_account(
            &crate::id(),
            warden,
            warden_map_account,
            ElusivWardenID::SIZE,
            &[&warden.key.to_bytes()],
        )?;

        let data = &mut warden_map_account.data.borrow_mut()[..];
        data.copy_from_slice(&warden_id.to_le_bytes());

        Ok(())
    }
}

#[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized, Debug, Clone)]
pub struct FixedLenString<const MAX_LEN: usize> {
    len: u64,
    data: [u8; MAX_LEN],
}

#[cfg(feature = "elusiv-client")]
impl<const MAX_LEN: usize> TryFrom<String> for FixedLenString<MAX_LEN> {
    type Error = std::io::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() > MAX_LEN {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "String is too long"))
        }

        let mut data = [0; MAX_LEN];
        data[..value.len()].copy_from_slice(value.as_bytes());

        Ok(
            Self {
                len: value.len() as u64,
                data,
            }
        )
    }
}

pub type Identifier = FixedLenString<256>;

#[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized, Debug, Clone)]
pub struct ElusivBasicWardenConfig {
    pub ident: Identifier,
    pub key: Pubkey,
    pub owner: Pubkey,

    pub addr: Ipv4Addr,
    pub port: u16,

    pub country: u16,
    pub asn: u32,

    pub version: [u16; 3],
    pub platform: Identifier,
}

#[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized, Debug, Clone)]
pub struct ElusivBasicWarden {
    pub warden_id: ElusivWardenID,
    pub config: ElusivBasicWardenConfig,
    pub lut: Pubkey,

    pub is_active: bool,

    pub join_timestamp: u64,
    pub activation_timestamp: u64,
}

/// An account associated to a single [`ElusivBasicWarden`]
#[elusiv_account(eager_type: true)]
pub struct BasicWardenAccount {
    pda_data: PDAAccountData,
    pub warden: ElusivBasicWarden,
}

#[derive(BorshDeserialize, BorshSerialize, BorshSerDeSized, Debug, Clone)]
pub struct WardenStatistics {
    pub activity: [u32; 366],
    pub total: u32,
}

const BASE_YEAR: u16 = 2022;
const YEARS_COUNT: usize = 100;
const WARDENS_COUNT: u32 = u32::MAX / YEARS_COUNT as u32;

impl WardenStatistics {
    pub fn inc(&self, day: u32) -> Result<&Self, ProgramError> {
        guard!(day < 366, ElusivWardenNetworkError::StatsError);

        self.total.checked_add(1)
            .ok_or(ElusivWardenNetworkError::Overflow)?;

        self.activity[day as usize].checked_add(1)
            .ok_or(ElusivWardenNetworkError::Overflow)?;

        Ok(self)
    }
}

/// An account associated to a single [`ElusivBasicWarden`] storing activity statistics for a single year
#[elusiv_account(eager_type: true)]
pub struct BasicWardenStatsAccount {
    pda_data: PDAAccountData,

    pub warden_id: ElusivWardenID,
    pub year: u16,

    pub store: WardenStatistics,
    pub send: WardenStatistics,
    pub migrate: WardenStatistics,
}

/// Returns the PDA and bump for an account mapping a warden pubkey to a [`ElusivWardenID`]
pub fn basic_warden_map_account_pda(pubkey: Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[&pubkey.to_bytes()], &crate::PROGRAM_ID)
}

pub fn stats_account_pda_offset(warden_id: ElusivWardenID, year: u16) -> u32 {
    assert!(year >= BASE_YEAR);
    assert!(warden_id < WARDENS_COUNT);

    (year - BASE_YEAR) as u32 * WARDENS_COUNT + warden_id
}