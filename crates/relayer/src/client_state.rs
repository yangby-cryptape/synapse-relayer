use core::time::Duration;

use ibc_proto::ibc::core::client::v1::IdentifiedClientState;
use ibc_proto::ibc::lightclients::tendermint::v1::ClientState as RawClientState;
#[cfg(test)]
use ibc_proto::ibc::mock::ClientState as RawMockClientState;
use ibc_proto::protobuf::Protobuf;
use serde::{Deserialize, Serialize};

use ibc_proto::google::protobuf::Any;
use ibc_relayer_types::clients::ics07_axon::client_state::ClientState as AxonClientState;
use ibc_relayer_types::clients::ics07_ckb::client_state::{
    ClientState as CkbClientState, CLIENT_STATE_TYPE_URL as CKB_CLIENT_STATE_TYPE_URL,
};
use ibc_relayer_types::clients::ics07_eth::client_state::{
    ClientState as EthClientState, CLIENT_STATE_TYPE_URL as ETH_CLIENT_STATE_TYPE_URL,
};
use ibc_relayer_types::clients::ics07_tendermint::client_state::{
    ClientState as TmClientState, UpgradeOptions as TmUpgradeOptions,
    TENDERMINT_CLIENT_STATE_TYPE_URL,
};
use ibc_relayer_types::core::ics02_client::client_state::{
    downcast_client_state, ClientState, UpgradeOptions,
};
use ibc_relayer_types::core::ics02_client::client_type::ClientType;
use ibc_relayer_types::core::ics02_client::error::Error;
use ibc_relayer_types::core::ics02_client::trust_threshold::TrustThreshold;

use ibc_relayer_types::core::ics24_host::error::ValidationError;
use ibc_relayer_types::core::ics24_host::identifier::{ChainId, ClientId};
#[cfg(test)]
use ibc_relayer_types::mock::client_state::MockClientState;
#[cfg(test)]
use ibc_relayer_types::mock::client_state::MOCK_CLIENT_STATE_TYPE_URL;
use ibc_relayer_types::Height;

use crate::error::Error as RelayerError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AnyUpgradeOptions {
    Tendermint(TmUpgradeOptions),

    #[cfg(test)]
    Mock(()),
}

impl AnyUpgradeOptions {
    fn as_tm_upgrade_options(&self) -> Option<&TmUpgradeOptions> {
        match self {
            AnyUpgradeOptions::Tendermint(tm) => Some(tm),
            #[cfg(test)]
            AnyUpgradeOptions::Mock(_) => None,
        }
    }
}

impl UpgradeOptions for AnyUpgradeOptions {}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AnyClientState {
    Tendermint(TmClientState),
    Eth(EthClientState),
    Ckb(CkbClientState),
    Axon(AxonClientState),

    #[cfg(test)]
    Mock(MockClientState),
}

impl AnyClientState {
    pub fn latest_height(&self) -> Height {
        match self {
            Self::Tendermint(tm_state) => tm_state.latest_height(),
            Self::Eth(state) => state.latest_height(),
            Self::Ckb(state) => state.latest_height(),
            Self::Axon(state) => state.latest_height(),

            #[cfg(test)]
            Self::Mock(mock_state) => mock_state.latest_height(),
        }
    }

    pub fn frozen_height(&self) -> Option<Height> {
        match self {
            Self::Tendermint(tm_state) => tm_state.frozen_height(),
            Self::Eth(state) => state.frozen_height(),
            Self::Ckb(state) => state.frozen_height(),
            Self::Axon(state) => state.frozen_height(),

            #[cfg(test)]
            Self::Mock(mock_state) => mock_state.frozen_height(),
        }
    }

    pub fn trust_threshold(&self) -> Option<TrustThreshold> {
        match self {
            AnyClientState::Tendermint(state) => Some(state.trust_threshold),
            AnyClientState::Eth(_) => None,
            AnyClientState::Ckb(_) => None,
            AnyClientState::Axon(_) => TrustThreshold::new(1, 2).ok(),

            #[cfg(test)]
            AnyClientState::Mock(_) => None,
        }
    }

    pub fn max_clock_drift(&self) -> Duration {
        match self {
            AnyClientState::Tendermint(state) => state.max_clock_drift,
            AnyClientState::Eth(_) => Duration::ZERO,
            AnyClientState::Ckb(_) => Duration::ZERO,
            AnyClientState::Axon(_) => Duration::ZERO,

            #[cfg(test)]
            AnyClientState::Mock(_) => Duration::new(0, 0),
        }
    }

    pub fn client_type(&self) -> ClientType {
        match self {
            Self::Tendermint(state) => state.client_type(),
            Self::Eth(state) => state.client_type(),
            Self::Ckb(state) => state.client_type(),
            Self::Axon(state) => state.client_type(),

            #[cfg(test)]
            Self::Mock(state) => state.client_type(),
        }
    }

    pub fn refresh_period(&self) -> Option<Duration> {
        match self {
            AnyClientState::Tendermint(tm_state) => tm_state.refresh_time(),
            AnyClientState::Eth(_) => None,
            AnyClientState::Ckb(_) => None,
            AnyClientState::Axon(_) => None,

            #[cfg(test)]
            AnyClientState::Mock(mock_state) => mock_state.refresh_time(),
        }
    }
}

impl Protobuf<Any> for AnyClientState {}

impl TryFrom<Any> for AnyClientState {
    type Error = Error;

    fn try_from(raw: Any) -> Result<Self, Self::Error> {
        match raw.type_url.as_str() {
            "" => Err(Error::empty_client_state_response()),

            TENDERMINT_CLIENT_STATE_TYPE_URL => Ok(AnyClientState::Tendermint(
                Protobuf::<RawClientState>::decode_vec(&raw.value)
                    .map_err(Error::decode_raw_client_state)?,
            )),

            #[cfg(test)]
            MOCK_CLIENT_STATE_TYPE_URL => Ok(AnyClientState::Mock(
                Protobuf::<RawMockClientState>::decode_vec(&raw.value)
                    .map_err(Error::decode_raw_client_state)?,
            )),

            _ => Err(Error::unknown_client_state_type(raw.type_url)),
        }
    }
}

impl From<AnyClientState> for Any {
    fn from(value: AnyClientState) -> Self {
        match value {
            AnyClientState::Tendermint(value) => Any {
                type_url: TENDERMINT_CLIENT_STATE_TYPE_URL.to_string(),
                value: Protobuf::<RawClientState>::encode_vec(&value)
                    .expect("encoding to `Any` from `AnyClientState::Tendermint`"),
            },
            AnyClientState::Eth(value) => {
                let json = serde_json::to_string(&value).expect("jsonify clientstate");
                Any {
                    type_url: ETH_CLIENT_STATE_TYPE_URL.to_owned(),
                    value: json.into_bytes(),
                }
            }
            AnyClientState::Ckb(value) => {
                let json = serde_json::to_string(&value).expect("jsonify clientstate");
                Any {
                    type_url: CKB_CLIENT_STATE_TYPE_URL.to_owned(),
                    value: json.into_bytes(),
                }
            }
            AnyClientState::Axon(_) => todo!(),
            #[cfg(test)]
            AnyClientState::Mock(value) => Any {
                type_url: MOCK_CLIENT_STATE_TYPE_URL.to_string(),
                value: Protobuf::<RawMockClientState>::encode_vec(&value)
                    .expect("encoding to `Any` from `AnyClientState::Mock`"),
            },
        }
    }
}

impl ClientState for AnyClientState {
    fn chain_id(&self) -> ChainId {
        match self {
            AnyClientState::Tendermint(tm_state) => tm_state.chain_id(),
            AnyClientState::Eth(state) => state.chain_id(),
            AnyClientState::Ckb(state) => state.chain_id(),
            AnyClientState::Axon(state) => state.chain_id(),

            #[cfg(test)]
            AnyClientState::Mock(mock_state) => mock_state.chain_id(),
        }
    }

    fn client_type(&self) -> ClientType {
        self.client_type()
    }

    fn latest_height(&self) -> Height {
        self.latest_height()
    }

    fn frozen_height(&self) -> Option<Height> {
        self.frozen_height()
    }

    fn upgrade(
        &mut self,
        upgrade_height: Height,
        upgrade_options: &dyn UpgradeOptions,
        chain_id: ChainId,
    ) {
        let upgrade_options = upgrade_options
            .as_any()
            .downcast_ref::<AnyUpgradeOptions>()
            .expect("UpgradeOptions not of type AnyUpgradeOptions");
        match self {
            AnyClientState::Tendermint(tm_state) => tm_state.upgrade(
                upgrade_height,
                upgrade_options.as_tm_upgrade_options().unwrap(),
                chain_id,
            ),
            AnyClientState::Eth(_) => todo!(),
            AnyClientState::Ckb(_) => todo!(),
            AnyClientState::Axon(_) => todo!(),

            #[cfg(test)]
            AnyClientState::Mock(mock_state) => {
                mock_state.upgrade(upgrade_height, upgrade_options, chain_id)
            }
        }
    }

    fn expired(&self, elapsed_since_latest: Duration) -> bool {
        match self {
            AnyClientState::Tendermint(tm_state) => tm_state.expired(elapsed_since_latest),
            AnyClientState::Eth(_) => todo!(),
            AnyClientState::Ckb(_) => false,
            AnyClientState::Axon(_) => false,

            #[cfg(test)]
            AnyClientState::Mock(mock_state) => mock_state.expired(elapsed_since_latest),
        }
    }
}

impl From<TmClientState> for AnyClientState {
    fn from(cs: TmClientState) -> Self {
        Self::Tendermint(cs)
    }
}

impl From<EthClientState> for AnyClientState {
    fn from(value: EthClientState) -> Self {
        Self::Eth(value)
    }
}

impl From<CkbClientState> for AnyClientState {
    fn from(value: CkbClientState) -> Self {
        Self::Ckb(value)
    }
}
impl From<AxonClientState> for AnyClientState {
    fn from(value: AxonClientState) -> Self {
        Self::Axon(value)
    }
}

impl<'a> TryFrom<&'a AnyClientState> for &'a TmClientState {
    type Error = RelayerError;

    fn try_from(value: &'a AnyClientState) -> Result<Self, Self::Error> {
        if let AnyClientState::Tendermint(value) = value {
            Ok(value)
        } else {
            Err(RelayerError::client_type_mismatch(
                ClientType::Tendermint,
                value.client_type(),
            ))
        }
    }
}

impl<'a> TryFrom<&'a AnyClientState> for &'a EthClientState {
    type Error = RelayerError;

    fn try_from(value: &'a AnyClientState) -> Result<Self, Self::Error> {
        if let AnyClientState::Eth(value) = value {
            Ok(value)
        } else {
            Err(RelayerError::client_type_mismatch(
                ClientType::Eth,
                value.client_type(),
            ))
        }
    }
}

impl<'a> TryFrom<&'a AnyClientState> for &'a CkbClientState {
    type Error = RelayerError;

    fn try_from(value: &'a AnyClientState) -> Result<Self, Self::Error> {
        if let AnyClientState::Ckb(value) = value {
            Ok(value)
        } else {
            Err(RelayerError::client_type_mismatch(
                ClientType::Ckb,
                value.client_type(),
            ))
        }
    }
}

impl<'a> TryFrom<&'a AnyClientState> for &'a AxonClientState {
    type Error = RelayerError;

    fn try_from(value: &'a AnyClientState) -> Result<Self, Self::Error> {
        if let AnyClientState::Axon(value) = value {
            Ok(value)
        } else {
            Err(RelayerError::client_type_mismatch(
                ClientType::Axon,
                value.client_type(),
            ))
        }
    }
}

#[cfg(test)]
impl From<MockClientState> for AnyClientState {
    fn from(cs: MockClientState) -> Self {
        Self::Mock(cs)
    }
}

impl From<&dyn ClientState> for AnyClientState {
    fn from(client_state: &dyn ClientState) -> Self {
        #[cfg(test)]
        if let Some(cs) = downcast_client_state::<MockClientState>(client_state) {
            return AnyClientState::from(*cs);
        }

        if let Some(cs) = downcast_client_state::<TmClientState>(client_state) {
            AnyClientState::from(cs.clone())
        } else {
            unreachable!()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub struct IdentifiedAnyClientState {
    pub client_id: ClientId,
    pub client_state: AnyClientState,
}

impl IdentifiedAnyClientState {
    pub fn new(client_id: ClientId, client_state: AnyClientState) -> Self {
        IdentifiedAnyClientState {
            client_id,
            client_state,
        }
    }
}

impl Protobuf<IdentifiedClientState> for IdentifiedAnyClientState {}

impl TryFrom<IdentifiedClientState> for IdentifiedAnyClientState {
    type Error = Error;

    fn try_from(raw: IdentifiedClientState) -> Result<Self, Self::Error> {
        Ok(IdentifiedAnyClientState {
            client_id: raw.client_id.parse().map_err(|e: ValidationError| {
                Error::invalid_raw_client_id(raw.client_id.clone(), e)
            })?,
            client_state: raw
                .client_state
                .ok_or_else(Error::missing_raw_client_state)?
                .try_into()?,
        })
    }
}

impl From<IdentifiedAnyClientState> for IdentifiedClientState {
    fn from(value: IdentifiedAnyClientState) -> Self {
        IdentifiedClientState {
            client_id: value.client_id.to_string(),
            client_state: Some(value.client_state.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use ibc_proto::google::protobuf::Any;
    use ibc_relayer_types::clients::ics07_tendermint::client_state::test_util::get_dummy_tendermint_client_state;
    use ibc_relayer_types::clients::ics07_tendermint::header::test_util::get_dummy_tendermint_header;
    use test_log::test;

    use super::AnyClientState;

    #[test]
    fn any_client_state_serialization() {
        let tm_client_state: AnyClientState =
            get_dummy_tendermint_client_state(get_dummy_tendermint_header()).into();

        let raw: Any = tm_client_state.clone().into();
        let tm_client_state_back = AnyClientState::try_from(raw).unwrap();
        assert_eq!(tm_client_state, tm_client_state_back);
    }
}
