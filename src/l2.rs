use std::ops::{Deref, DerefMut};

use alloy::{
    network::{Ethereum, EthereumWallet, NetworkWallet},
    providers::{
        fillers::{ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller, WalletFiller},
        Identity, ProviderBuilder, RootProvider,
    },
    signers::local::PrivateKeySigner,
    transports::http::{Client, Http},
};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::{seed::Seed, settings::SETTINGS};

// alloy moment 💀
type Provider = FillProvider<
    JoinFill<
        JoinFill<JoinFill<JoinFill<Identity, GasFiller>, NonceFiller>, ChainIdFiller>,
        WalletFiller<EthereumWallet>,
    >,
    RootProvider<Http<Client>>,
    Http<Client>,
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
        let l2_private_bytes = {
            let mut hasher = Sha256::new();
            hasher.update(b"alpen labs faucet l2 wallet 2024");
            hasher.update(seed);
            hasher.finalize()
        };

        let signer = PrivateKeySigner::from_field_bytes(&l2_private_bytes).expect("valid slice");

        let wallet = EthereumWallet::from(signer);

        info!(
            "L2 faucet address: {}",
            <EthereumWallet as NetworkWallet<Ethereum>>::default_signer_address(&wallet)
        );

        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_http(
                SETTINGS
                    .l2_http_endpoint
                    .parse()
                    .map_err(|_| L2EndpointParseError)?,
            );
        Ok(Self(provider))
    }
}
