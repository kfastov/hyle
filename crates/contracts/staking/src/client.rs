use std::any::Any;

use client_sdk::{
    helpers::{risc0::Risc0Prover, ClientSdkExecutor},
    transaction_builder::{ProvableBlobTx, StateUpdater, TxExecutorBuilder},
};
use sdk::{
    api::APIStaking, utils::as_hyle_output, ContractName, Digestable, HyleOutput, StakingAction,
    ValidatorPublicKey,
};

use crate::{execute, state::Staking};

pub mod metadata {
    pub const STAKING_ELF: &[u8] = include_bytes!("../staking.img");
    pub const PROGRAM_ID: [u8; 32] = sdk::str_to_u8(include_str!("../staking.txt"));
}
use metadata::*;

struct StakingPseudoExecutor {}
impl ClientSdkExecutor for StakingPseudoExecutor {
    fn execute(
        &self,
        contract_input: &sdk::ContractInput,
    ) -> anyhow::Result<(Box<dyn Any>, HyleOutput)> {
        let mut res = execute(contract_input.clone());
        let output = as_hyle_output(contract_input.clone(), &mut res);
        match res {
            Ok(res) => Ok((Box::new(res.1.clone()), output)),
            Err(e) => Err(anyhow::anyhow!(e)),
        }
    }
}

impl Staking {
    pub fn setup_builder<S: StateUpdater>(
        &self,
        contract_name: ContractName,
        builder: &mut TxExecutorBuilder<S>,
    ) {
        builder.init_with(
            contract_name,
            self.as_digest(),
            StakingPseudoExecutor {},
            Risc0Prover::new(STAKING_ELF),
        );
    }
}

impl From<Staking> for APIStaking {
    fn from(val: Staking) -> Self {
        APIStaking {
            stakes: val.stakes,
            rewarded: val.rewarded,
            bonded: val.bonded,
            delegations: val.delegations,
            total_bond: val.total_bond,
        }
    }
}

impl From<APIStaking> for Staking {
    fn from(val: APIStaking) -> Self {
        Staking {
            stakes: val.stakes,
            rewarded: val.rewarded,
            bonded: val.bonded,
            delegations: val.delegations,
            total_bond: val.total_bond,
        }
    }
}

impl Staking {
    pub fn to_bytes(&self) -> Vec<u8> {
        bincode::encode_to_vec(self, bincode::config::standard())
            .expect("Failed to encode Balances")
    }
}

pub fn stake(
    builder: &mut ProvableBlobTx,
    contract_name: ContractName,
    amount: u128,
) -> anyhow::Result<()> {
    builder
        .add_action(contract_name, StakingAction::Stake { amount }, None, None)?
        .with_private_input(|state: &Staking| -> anyhow::Result<Vec<u8>> { Ok(state.to_bytes()) });
    Ok(())
}

pub fn delegate(
    builder: &mut ProvableBlobTx,
    contract_name: ContractName,
    validator: ValidatorPublicKey,
) -> anyhow::Result<()> {
    builder
        .add_action(
            contract_name,
            StakingAction::Delegate {
                validator: validator.clone(),
            },
            None,
            None,
        )?
        .with_private_input(|state: &Staking| -> anyhow::Result<Vec<u8>> { Ok(state.to_bytes()) });
    Ok(())
}
