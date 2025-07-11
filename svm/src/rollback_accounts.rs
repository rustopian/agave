use {
    crate::nonce_info::NonceInfo,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_transaction_context::TransactionAccount,
};

/// Captured account state used to rollback account state for nonce and fee
/// payer accounts after a failed executed transaction.
#[derive(PartialEq, Eq, Debug, Clone)]
pub enum RollbackAccounts {
    FeePayerOnly {
        fee_payer: TransactionAccount,
    },
    SameNonceAndFeePayer {
        nonce: TransactionAccount,
    },
    SeparateNonceAndFeePayer {
        nonce: TransactionAccount,
        fee_payer: TransactionAccount,
    },
}

#[cfg(feature = "dev-context-only-utils")]
impl Default for RollbackAccounts {
    fn default() -> Self {
        Self::FeePayerOnly {
            fee_payer: TransactionAccount::default(),
        }
    }
}

/// Rollback accounts iterator.
/// This struct is created by the `RollbackAccounts::iter`.
pub struct RollbackAccountsIter<'a> {
    fee_payer: Option<&'a TransactionAccount>,
    nonce: Option<&'a TransactionAccount>,
}

impl<'a> Iterator for RollbackAccountsIter<'a> {
    type Item = &'a TransactionAccount;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(fee_payer) = self.fee_payer.take() {
            return Some(fee_payer);
        }
        if let Some(nonce) = self.nonce.take() {
            return Some(nonce);
        }
        None
    }
}

impl<'a> IntoIterator for &'a RollbackAccounts {
    type Item = &'a TransactionAccount;
    type IntoIter = RollbackAccountsIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl RollbackAccounts {
    pub(crate) fn new(
        nonce: Option<NonceInfo>,
        fee_payer_address: Pubkey,
        mut fee_payer_account: AccountSharedData,
        fee_payer_loaded_rent_epoch: Epoch,
    ) -> Self {
        if let Some(nonce) = nonce {
            if &fee_payer_address == nonce.address() {
                // `nonce` contains an AccountSharedData which has already been advanced to the current DurableNonce
                // `fee_payer_account` is an AccountSharedData as it currently exists on-chain
                // thus if the nonce account is being used as the fee payer, we need to update that data here
                // so we capture both the data change for the nonce and the lamports/rent epoch change for the fee payer
                fee_payer_account.set_data_from_slice(nonce.account().data());

                RollbackAccounts::SameNonceAndFeePayer {
                    nonce: (fee_payer_address, fee_payer_account),
                }
            } else {
                RollbackAccounts::SeparateNonceAndFeePayer {
                    nonce: (nonce.address, nonce.account),
                    fee_payer: (fee_payer_address, fee_payer_account),
                }
            }
        } else {
            // When rolling back failed transactions which don't use nonces, the
            // runtime should not update the fee payer's rent epoch so reset the
            // rollback fee payer account's rent epoch to its originally loaded
            // rent epoch value. In the future, a feature gate could be used to
            // alter this behavior such that rent epoch updates are handled the
            // same for both nonce and non-nonce failed transactions.
            fee_payer_account.set_rent_epoch(fee_payer_loaded_rent_epoch);
            RollbackAccounts::FeePayerOnly {
                fee_payer: (fee_payer_address, fee_payer_account),
            }
        }
    }

    /// Number of accounts tracked for rollback
    pub fn count(&self) -> usize {
        match self {
            Self::FeePayerOnly { .. } | Self::SameNonceAndFeePayer { .. } => 1,
            Self::SeparateNonceAndFeePayer { .. } => 2,
        }
    }

    /// Iterator over accounts tracked for rollback.
    pub fn iter(&self) -> RollbackAccountsIter<'_> {
        match self {
            Self::FeePayerOnly { fee_payer } => RollbackAccountsIter {
                fee_payer: Some(fee_payer),
                nonce: None,
            },
            Self::SameNonceAndFeePayer { nonce } => RollbackAccountsIter {
                fee_payer: None,
                nonce: Some(nonce),
            },
            Self::SeparateNonceAndFeePayer { nonce, fee_payer } => RollbackAccountsIter {
                fee_payer: Some(fee_payer),
                nonce: Some(nonce),
            },
        }
    }

    /// Size of accounts tracked for rollback, used when calculating the actual
    /// cost of transaction processing in the cost model.
    pub fn data_size(&self) -> usize {
        let mut total_size: usize = 0;
        for (_, account) in self.iter() {
            total_size = total_size.saturating_add(account.data().len());
        }
        total_size
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_account::{ReadableAccount, WritableAccount},
        solana_hash::Hash,
        solana_nonce::{
            state::{Data as NonceData, DurableNonce, State as NonceState},
            versions::Versions as NonceVersions,
        },
        solana_sdk_ids::system_program,
    };

    #[test]
    fn test_new_fee_payer_only() {
        let fee_payer_address = Pubkey::new_unique();
        let fee_payer_account = AccountSharedData::new(100, 0, &Pubkey::default());
        let fee_payer_rent_epoch = fee_payer_account.rent_epoch();

        let rent_epoch_updated_fee_payer_account = {
            let mut account = fee_payer_account.clone();
            account.set_lamports(fee_payer_account.lamports());
            account.set_rent_epoch(fee_payer_rent_epoch + 1);
            account
        };

        let rollback_accounts = RollbackAccounts::new(
            None,
            fee_payer_address,
            rent_epoch_updated_fee_payer_account,
            fee_payer_rent_epoch,
        );

        let expected_fee_payer = (fee_payer_address, fee_payer_account);
        match rollback_accounts {
            RollbackAccounts::FeePayerOnly { fee_payer } => {
                assert_eq!(expected_fee_payer, fee_payer);
            }
            _ => panic!("Expected FeePayerOnly variant"),
        }
    }

    #[test]
    fn test_new_same_nonce_and_fee_payer() {
        let nonce_address = Pubkey::new_unique();
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let lamports_per_signature = 42;
        let nonce_account = AccountSharedData::new_data(
            43,
            &NonceVersions::new(NonceState::Initialized(NonceData::new(
                Pubkey::default(),
                durable_nonce,
                lamports_per_signature,
            ))),
            &system_program::id(),
        )
        .unwrap();

        let rent_epoch_updated_fee_payer_account = {
            let mut account = nonce_account.clone();
            account.set_lamports(nonce_account.lamports());
            account
        };

        let nonce = NonceInfo::new(nonce_address, rent_epoch_updated_fee_payer_account.clone());
        let rollback_accounts = RollbackAccounts::new(
            Some(nonce),
            nonce_address,
            rent_epoch_updated_fee_payer_account,
            u64::MAX, // ignored
        );

        let expected_rollback_accounts = RollbackAccounts::SameNonceAndFeePayer {
            nonce: (nonce_address, nonce_account),
        };

        assert_eq!(expected_rollback_accounts, rollback_accounts);
    }

    #[test]
    fn test_separate_nonce_and_fee_payer() {
        let nonce_address = Pubkey::new_unique();
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let lamports_per_signature = 42;
        let nonce_account = AccountSharedData::new_data(
            43,
            &NonceVersions::new(NonceState::Initialized(NonceData::new(
                Pubkey::default(),
                durable_nonce,
                lamports_per_signature,
            ))),
            &system_program::id(),
        )
        .unwrap();

        let fee_payer_address = Pubkey::new_unique();
        let fee_payer_account = AccountSharedData::new(44, 0, &Pubkey::default());

        let rent_epoch_updated_fee_payer_account = {
            let mut account = fee_payer_account.clone();
            account.set_lamports(fee_payer_account.lamports());
            account
        };

        let nonce = NonceInfo::new(nonce_address, nonce_account.clone());
        let rollback_accounts = RollbackAccounts::new(
            Some(nonce),
            fee_payer_address,
            rent_epoch_updated_fee_payer_account.clone(),
            u64::MAX, // ignored
        );

        let expected_nonce = (nonce_address, nonce_account);
        let expected_fee_payer = (fee_payer_address, fee_payer_account);
        match rollback_accounts {
            RollbackAccounts::SeparateNonceAndFeePayer { nonce, fee_payer } => {
                assert_eq!(expected_nonce, nonce);
                assert_eq!(expected_fee_payer, fee_payer);
            }
            _ => panic!("Expected SeparateNonceAndFeePayer variant"),
        }
    }
}
