use std::ops::{Deref, DerefMut};

use alloy::{
    network::{Ethereum, EthereumWallet, NetworkWallet},
    providers::{
        fillers::{
            BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller,
            WalletFiller,
        },
        Identity, ProviderBuilder, RootProvider,
    },
    signers::local::PrivateKeySigner,
};
use bdk_wallet::bitcoin::{
                Network,
                secp256k1::Secp256k1,
                bip32::{DerivationPath,
                    ChildNumber,
                    Xpriv,
                }
            };
use tracing::info;

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

impl L2Wallet {
    pub fn new(seed: &Seed) -> Result<Self, L2EndpointParseError> {
        let derivation_path = DerivationPath::master().extend(&[
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
        ]);

        // Network choice affects how extended public and private keys are serialized. See
        // https://github.com/bitcoin/bips/blob/master/bip-0032.mediawiki#serialization-format.
        // Given the popularity of MetaMask, we follow their example (they always hardcode mainnet)
        // and hardcode Network::Bitcoin (mainnet) for EVM-based wallet.
        let master_key = Xpriv::new_master(Network::Bitcoin, seed).expect("valid xpriv");

        // Derive the child key for the given path
        let derived_key = master_key.derive_priv(&Secp256k1::new(), &derivation_path).unwrap();
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
}
