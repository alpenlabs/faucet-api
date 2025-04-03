use std::ops::{Deref, DerefMut};

use alloy::{
    network::{Ethereum, EthereumWallet, NetworkWallet},
    primitives::Address,
    providers::{
        fillers::{
            BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller,
            WalletFiller,
        },
        Identity, Provider as AProvider, ProviderBuilder, RootProvider, WalletProvider,
    },
    signers::local::PrivateKeySigner,
};
use bdk_wallet::bitcoin::{
    bip32::{ChildNumber, DerivationPath, Xpriv},
    secp256k1::Secp256k1,
    Network,
};
use bip39::Mnemonic;
use tracing::{error, info};

use crate::{seed::Seed, settings::SETTINGS};

// alloy moment 💀
type Provider = FillProvider<
    JoinFill<
        JoinFill<
            Identity,
            JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>,
        >,
        WalletFiller<EthereumWallet>,
    >,
    RootProvider<Ethereum>,
    Ethereum,
>;

pub struct L2Wallet(Provider);

impl DerefMut for L2Wallet {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Deref for L2Wallet {
    type Target = Provider;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct L2EndpointParseError;

/// Faucet api [`DerivationPath`](bdk_wallet::bitcoin::bip32::DerivationPath) for L2 EVM wallet
///
/// This corresponds to the path: `m/44'/60'/0'/0/0`.
const BIP44_STRATA_EVM_WALLET_PATH: &[ChildNumber] = &[
    // Purpose index for HD wallets.
    ChildNumber::Hardened { index: 44 },
    // Coin type index for Ethereum mainnet
    ChildNumber::Hardened { index: 60 },
    // Account index for user wallets.
    ChildNumber::Hardened { index: 0 },
    // Change index for receiving (external) addresses.
    ChildNumber::Normal { index: 0 },
    // Address index.
    ChildNumber::Normal { index: 0 },
];

impl L2Wallet {
    pub fn new(seed: &Seed) -> Result<Self, L2EndpointParseError> {
        let derivation_path = DerivationPath::master().extend(BIP44_STRATA_EVM_WALLET_PATH);
        let mnemonic = Mnemonic::from_entropy(seed).expect("valid entropy");
        // We do not use a passphrase.
        let bip39_seed = mnemonic.to_seed("");

        // Network choice affects how extended public and private keys are serialized.
        // See https://github.com/bitcoin/bips/blob/master/bip-0032.mediawiki#serialization-format.
        // Given the popularity of MetaMask, we follow their example (they always
        // hardcode mainnet) and hardcode Network::Bitcoin (mainnet) for
        // EVM-based wallet.
        let master_key = Xpriv::new_master(Network::Bitcoin, &bip39_seed).expect("valid xpriv");

        // Derive the child key for the given path
        let derived_key = master_key
            .derive_priv(&Secp256k1::new(), &derivation_path)
            .unwrap();
        let signer =
            PrivateKeySigner::from_slice(derived_key.private_key.secret_bytes().as_slice())
                .expect("valid slice");

        let wallet = EthereumWallet::from(signer);

        info!(
            "L2 faucet address: {}",
            <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&wallet)
        );

        let provider = ProviderBuilder::new().wallet(wallet).on_http(
            SETTINGS
                .l2_http_endpoint
                .parse()
                .map_err(|_| L2EndpointParseError)?,
        );
        Ok(Self(provider))
    }

    pub fn default_signer_address(&self) -> Address {
        self.0.default_signer_address()
    }

    pub async fn get_default_signer_balance(&self) -> Result<u128, String> {
        let signer_addr = self.0.default_signer_address();
        match self.0.get_balance(signer_addr).await {
            Ok(x) => Ok(x.to()),
            Err(e) => {
                error!("Could not fetch l2 balance {:?}", e);
                Err("Could not fetch l2 balance".to_string())
            }
        }
    }
}
