use ckb_jsonrpc_types::{OutputsValidator, TransactionView as JsonTx};
use ckb_sdk::{Address, AddressPayload, NetworkType};
use ckb_types::core::TransactionView;
use ckb_types::packed::CellOutput;
use ckb_types::prelude::*;
use eth2_types::MainnetEthSpec;
use eth_light_client_in_ckb_verification::types::{
    packed::Client as PackedClient, packed::ClientInfo as PackedClientInfo,
    packed::ClientTypeArgs as PackedClientTypeArgs, packed::Hash as PackedHash,
    packed::ProofUpdate as PackedProofUpdate, prelude::Unpack,
};
use ibc_proto::ibc::apps::fee::v1::{
    QueryIncentivizedPacketRequest, QueryIncentivizedPacketResponse,
};
use ibc_relayer_storage::prelude::{StorageAsMMRStore as _, StorageReader as _};
use ibc_relayer_storage::{Slot, Storage};
use ibc_relayer_types::applications::ics31_icq::response::CrossChainQueryResponse;
use ibc_relayer_types::clients::ics07_ckb::{
    client_state::ClientState as CkbClientState,
    consensus_state::ConsensusState as CkbConsensusState, header::Header as CkbHeader,
    light_block::LightBlock as CkbLightBlock,
};
use ibc_relayer_types::clients::ics07_eth::{
    client_state::ClientState as EthClientState, types::Update as EthUpdate,
};
use ibc_relayer_types::{
    core::{
        ics02_client::events::UpdateClient,
        ics03_connection::connection::{ConnectionEnd, IdentifiedConnectionEnd},
        ics04_channel::{
            channel::{ChannelEnd, IdentifiedChannelEnd},
            packet::Sequence,
        },
        ics23_commitment::{commitment::CommitmentPrefix, merkle::MerkleProof},
        ics24_host::identifier::{ChannelId, ConnectionId, PortId},
    },
    signer::Signer,
    Height as ICSHeight,
};
use semver::Version;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tendermint_light_client::errors::Error as LightClientError;
use tendermint_rpc::endpoint::broadcast::tx_sync::Response;
use tokio::runtime::Runtime as TokioRuntime;

#[cfg(test)]
use crate::keyring::Store;

use crate::{
    account::Balance,
    chain::cosmos::encode::key_pair_to_signer,
    chain::endpoint::{ChainEndpoint, ChainStatus, HealthCheck},
    client_state::{AnyClientState, IdentifiedAnyClientState},
    config::ckb::ChainConfig as CkbChainConfig,
    config::ChainConfig,
    // config::GLOBAL_CONFIG_PATH,
    consensus_state::AnyConsensusState,
    denom::DenomTrace,
    error::Error,
    event::IbcEventWithHeight,
    keyring::{KeyRing, Secp256k1KeyPair},
    misbehaviour::MisbehaviourEvidence,
};

use super::requests::{CrossChainQueryRequest, QueryConsensusStateHeightsRequest};
use super::tracking::{NonCosmosTrackingId as NonCosmos, TrackedMsgs, TrackingId};
use super::{
    client::ClientSettings,
    requests::{
        IncludeProof, QueryChannelClientStateRequest, QueryChannelRequest, QueryChannelsRequest,
        QueryClientConnectionsRequest, QueryClientStateRequest, QueryClientStatesRequest,
        QueryConnectionRequest, QueryConnectionsRequest, QueryConsensusStateRequest,
        QueryHostConsensusStateRequest, QueryNextSequenceReceiveRequest,
        QueryPacketAcknowledgementRequest, QueryPacketAcknowledgementsRequest,
        QueryPacketCommitmentsRequest, QueryUnreceivedAcksRequest, QueryUnreceivedPacketsRequest,
        QueryUpgradedClientStateRequest, QueryUpgradedConsensusStateRequest,
    },
};

mod assembler;
mod communication;
mod helper;
pub mod sighash;
mod signer;
pub mod utils;

#[cfg(test)]
pub mod mock_rpc_client;
#[cfg(not(test))]
pub mod rpc_client;
#[cfg(test)]
pub use mock_rpc_client as rpc_client;

#[cfg(test)]
mod tests;

pub mod prelude {
    pub use super::{
        assembler::{TxAssembler, UpdateCells},
        communication::{CkbReader, CkbWriter, Response},
        helper::{CellSearcher, TxCompleter},
    };
}

use assembler::TxAssembler;

use prelude::{CkbReader as _, CkbWriter as _, UpdateCells};

use rpc_client::RpcClient;

// Ref: https://github.com/satoshilabs/slips/pull/621
pub const HD_PATH: &str = "m/44'/309'/0'/0/0";

pub struct CkbChain {
    pub rt: Arc<TokioRuntime>,
    pub rpc_client: Arc<RpcClient>,
    pub config: CkbChainConfig,
    pub keybase: KeyRing<Secp256k1KeyPair>,
    // TODO the spec of Ethereum should be selectable.
    pub storage: Storage<MainnetEthSpec>,

    pub cached_network: RwLock<Option<NetworkType>>,
    pub cached_tx_assembler_address: RwLock<Option<Address>>,
    pub cached_onchain_packed_client: Option<PackedClient>,
}

impl CkbChain {
    fn create_eth_multi_client(
        &mut self,
        mut header_updates: Vec<EthUpdate>,
    ) -> Result<Vec<IbcEventWithHeight>, Error> {
        let chain_id = self.id().to_string();
        let minimal_updates_count = self.config.minimal_updates_count;
        let client_type_args = &self.config.client_type_args;

        if let Some(type_id) = client_type_args.type_id.as_ref() {
            let client_type_args: PackedClientTypeArgs = {
                let type_id = PackedHash::from_slice(type_id.0.as_slice()).expect("build type id");
                PackedClientTypeArgs::new_builder()
                    .cells_count(client_type_args.cells_count.into())
                    .type_id(type_id)
                    .build()
            };

            let update_cells = self.rt.block_on(self.rpc_client.fetch_update_cells(
                &self.config.lightclient_contract_typeargs,
                &client_type_args,
            ))?;
            if let Some(UpdateCells {
                oldest: _,
                latest,
                info: _,
            }) = update_cells
            {
                let latest_client = PackedClient::new_unchecked(latest.output_data);
                self.cached_onchain_packed_client = Some(latest_client.clone());

                let onchain_base_slot = latest_client.minimal_slot().unpack();
                // This is for reporting that clients have been created at that slot.
                // TODO: better error type to match the semantic.
                return Err(Error::light_client_verification(
                    chain_id,
                    LightClientError::missing_last_block_id(utils::into_height(onchain_base_slot)),
                ));
            } else {
                return Err(Error::other_error(
                    "no multi-client cells found for config".to_owned(),
                ));
            }
        }

        let client_count = {
            let cells_count = client_type_args.cells_count;
            cells_count.checked_sub(1).expect("invalid cells_count")
        };

        let (packed_client, packed_proof_update, prev_slot_opt) =
            self.get_new_client_and_proof(&chain_id, &mut header_updates, minimal_updates_count)?;
        let clients = (0..client_count)
            .map(|i| packed_client.clone().as_builder().id(i.into()).build())
            .collect::<Vec<_>>();
        let client_info = PackedClientInfo::new_builder()
            .last_id(0.into())
            .minimal_updates_count(minimal_updates_count.into())
            .build();

        let tx_assembler_address = self.tx_assembler_address()?;
        let (tx, inputs, type_id) =
            self.rt
                .block_on(self.rpc_client.assemble_create_multi_client_transaction(
                    &tx_assembler_address,
                    clients,
                    client_info,
                    &self.config.lightclient_lock_typeargs,
                    &self.config.lightclient_contract_typeargs,
                    packed_proof_update,
                ))?;
        self.sign_and_send_transaction(tx, inputs).map_err(|err| {
            if let Err(err) = self.storage.rollback_to(prev_slot_opt) {
                return err.into();
            }
            err
        })?;

        // TODO: Write back the type id to config.
        tracing::info!("new type_id: {}", type_id);
        self.config.client_type_args.type_id = Some(type_id);

        self.print_status_log()?;
        Ok(vec![])
    }

    fn update_eth_multi_client(
        &mut self,
        mut header_updates: Vec<EthUpdate>,
    ) -> Result<Vec<IbcEventWithHeight>, Error> {
        let chain_id = self.id().to_string();
        let client_type_args: PackedClientTypeArgs = {
            let Some(type_id) = self.config.client_type_args.type_id.as_ref()
            else {
                // TODO: better error
                return Err(Error::other_error("no type id in client type args".to_owned()));
            };
            let type_id = PackedHash::from_slice(type_id.0.as_slice()).expect("build type id");
            PackedClientTypeArgs::new_builder()
                .cells_count(self.config.client_type_args.cells_count.into())
                .type_id(type_id)
                .build()
        };

        let Some(update_cells) = self
            .rt
            .block_on(
                self
                    .rpc_client
                    .fetch_update_cells(
                        &self.config.lightclient_contract_typeargs,
                        &client_type_args
                    )
            )?
        else {
            return Err(Error::other_error("no multi-client cells found".to_owned()));
        };

        let latest_client = PackedClient::new_unchecked(update_cells.latest.output_data.clone());
        self.cached_onchain_packed_client = Some(latest_client);

        let minimal_updates_count = {
            let client_info =
                PackedClientInfo::new_unchecked(update_cells.info.output_data.clone());
            u8::from(client_info.minimal_updates_count().as_reader())
        };

        let (mut updated_client, packed_proof_update, prev_slot_opt) =
            self.get_new_client_and_proof(&chain_id, &mut header_updates, minimal_updates_count)?;
        updated_client = {
            let oldest_client =
                PackedClient::new_unchecked(update_cells.oldest.output_data.clone());
            updated_client.as_builder().id(oldest_client.id()).build()
        };

        let tx_assembler_address = self.tx_assembler_address()?;
        let (tx, inputs) =
            self.rt
                .block_on(self.rpc_client.assemble_update_multi_client_transaction(
                    &tx_assembler_address,
                    update_cells,
                    updated_client,
                    &client_type_args,
                    &self.config.lightclient_lock_typeargs,
                    &self.config.lightclient_contract_typeargs,
                    packed_proof_update,
                ))?;
        self.sign_and_send_transaction(tx, inputs).map_err(|err| {
            if let Err(err) = self.storage.rollback_to(prev_slot_opt) {
                return err.into();
            }
            err
        })?;

        self.print_status_log()?;
        Ok(vec![])
    }

    fn get_new_client_and_proof(
        &self,
        chain_id: &str,
        header_updates: &mut Vec<EthUpdate>,
        minimal_updates_count: u8,
    ) -> Result<(PackedClient, PackedProofUpdate, Option<Slot>), Error> {
        utils::align_native_and_onchain_updates(
            chain_id,
            header_updates,
            &self.storage,
            self.cached_onchain_packed_client.as_ref(),
        )?;
        let (prev_slot_opt, new_client, packed_proof_update) =
            utils::get_verified_packed_client_and_proof_update(
                chain_id,
                header_updates,
                &self.storage,
                self.cached_onchain_packed_client.as_ref(),
            )?;
        if new_client.maximal_slot().unpack() - new_client.minimal_slot().unpack() + 1
            < minimal_updates_count as u64
        {
            if let Err(err) = self.storage.rollback_to(prev_slot_opt) {
                return Err(err.into());
            }
            // TODO: This may require some handling outside to retry.
            return Err(Error::other_error(
                "not enough updates to update multi-client".to_owned(),
            ));
        }
        Ok((new_client, packed_proof_update, prev_slot_opt))
    }

    pub fn sign_and_send_transaction(
        &mut self,
        tx: TransactionView,
        inputs: Vec<CellOutput>,
    ) -> Result<(), Error> {
        let key: Secp256k1KeyPair = self
            .keybase
            .get_key(&self.config.key_name)
            .map_err(Error::key_base)?
            .into_ckb_keypair(self.network()?);
        let tx = signer::sign(tx, &inputs, vec![], key).map_err(Error::key_base)?;

        let task = async {
            let send_res = self
                .rpc_client
                .send_transaction(&tx.data().into(), Some(OutputsValidator::Passthrough))
                .await;
            let hash = match send_res {
                Ok(hash) => Ok(hash),
                Err(e) => {
                    let pool_log = utils::collect_ckb_tx_pool_info_on_duplicate_tx(
                        self.rpc_client.as_ref(),
                        &e,
                    )
                    .await
                    .unwrap_or_default();
                    let tx_info = format!(
                        "== transaction for debugging is below ==\n{}",
                        serde_json::to_string(&JsonTx::from(tx)).expect("jsonify ckb tx")
                    );
                    Err(Error::send_tx(format!("{e}\n{pool_log}\n{tx_info}\n")))
                }
            }?;

            tracing::info!(
                "ckb send_transaction success: {}, wait committed to block",
                hex::encode(&hash)
            );

            utils::wait_ckb_transaction_committed(
                &self.rpc_client,
                hash,
                Duration::from_secs(3),
                0,
                Duration::from_secs(60),
            )
            .await?;
            tracing::info!("transaction committed to block");
            Ok(())
        };
        self.rt.block_on(task)
    }

    pub fn network(&self) -> Result<NetworkType, Error> {
        let cached_network_opt: Option<NetworkType> =
            *self.cached_network.read().map_err(Error::other)?;
        let network = if let Some(network) = cached_network_opt {
            network
        } else {
            let network = {
                let chain_info = self
                    .rt
                    .block_on(self.rpc_client.get_blockchain_info())
                    .map_err(|e| Error::rpc_response(e.to_string()))?;
                if chain_info.chain == "ckb" {
                    NetworkType::Mainnet
                } else if chain_info.chain == "ckb_testnet" {
                    NetworkType::Testnet
                } else {
                    NetworkType::Dev
                }
            };
            *self.cached_network.write().map_err(Error::other)? = Some(network);
            network
        };
        Ok(network)
    }

    pub fn tx_assembler_address(&self) -> Result<Address, Error> {
        let cached_address = self
            .cached_tx_assembler_address
            .read()
            .map_err(Error::other)?
            .clone();
        let address = if let Some(address) = cached_address {
            address
        } else {
            let network = self.network()?;
            let key: Secp256k1KeyPair = self
                .keybase
                .get_key(&self.config.key_name)
                .map_err(Error::key_base)?;
            let address_payload = AddressPayload::from_pubkey(&key.public_key);
            let address = Address::new(network, address_payload, true);
            *self
                .cached_tx_assembler_address
                .write()
                .map_err(Error::other)? = Some(address.clone());
            address
        };
        Ok(address)
    }

    fn print_status_log(&self) -> Result<(), Error> {
        let contract_typeid_args = &self.config.lightclient_contract_typeargs;
        let client_type_args = &self.config.client_type_args;

        let mut status_log = String::new();

        if let Some(type_id) = client_type_args.type_id.as_ref() {
            let packed_client_type_args: PackedClientTypeArgs = {
                let type_id = PackedHash::from_slice(type_id.0.as_slice()).expect("build type id");
                PackedClientTypeArgs::new_builder()
                    .cells_count(client_type_args.cells_count.into())
                    .type_id(type_id)
                    .build()
            };
            let clients_and_info_opt = self.rt.block_on(
                self.rpc_client
                    .fetch_clients_and_info(contract_typeid_args, &packed_client_type_args),
            )?;
            if let Some((mut clients, info)) = clients_and_info_opt {
                clients.sort_by_key(|c| u8::from(c.id().as_reader()));
                let clients_msg = clients
                    .iter()
                    .map(|c| format!("{}", c.unpack()))
                    .collect::<Vec<String>>()
                    .join("\n");

                let info_msg = format!("{}", info.unpack());
                status_log += &format!("on-chain status:\n{clients_msg}\n{info_msg}\n")
            } else {
                status_log += "on-chain status: NONE, ";
            }
        } else {
            status_log += "on-chain status: NONE, ";
        }

        if let (Some(start_slot), Some(end_slot)) = (
            self.storage.get_base_beacon_header_slot()?,
            self.storage.get_tip_beacon_header_slot()?,
        ) {
            status_log += &format!("native status: [{start_slot}, {end_slot}]");
        } else {
            status_log += "native status: NONE";
        }
        tracing::info!("[STATUS] {status_log}");
        Ok(())
    }
}

impl ChainEndpoint for CkbChain {
    type LightBlock = CkbLightBlock;
    type Header = CkbHeader;
    type ConsensusState = CkbConsensusState;
    type ClientState = CkbClientState;
    type SigningKeyPair = Secp256k1KeyPair;

    fn config(&self) -> ChainConfig {
        ChainConfig::Ckb(self.config.clone())
    }

    fn bootstrap(config: ChainConfig, rt: Arc<TokioRuntime>) -> Result<Self, Error> {
        let config: CkbChainConfig = config.try_into()?;
        let rpc_client = Arc::new(RpcClient::new(&config.ckb_rpc, &config.ckb_indexer_rpc));
        let storage = Storage::new(&config.data_dir)?;

        #[cfg(not(test))]
        {
            use ckb_sdk::constants::TYPE_ID_CODE_HASH;
            use prelude::CellSearcher;
            use sighash::init_sighash_celldep;

            rt.block_on(init_sighash_celldep(rpc_client.as_ref()))?;

            // check if contract and lock type_id_args are on-chain deployed
            let contract_cell = rt.block_on(rpc_client.search_cell_by_typescript(
                &TYPE_ID_CODE_HASH.pack(),
                &config.lightclient_contract_typeargs.as_bytes().to_owned(),
            ))?;
            if contract_cell.is_none() {
                return Err(Error::other_error(
                    "invalid `lightclient_contract_typeargs` option".to_owned(),
                ));
            }
            let lock_cell = rt.block_on(rpc_client.search_cell_by_typescript(
                &TYPE_ID_CODE_HASH.pack(),
                &config.lightclient_lock_typeargs.as_bytes().to_owned(),
            ))?;
            if lock_cell.is_none() {
                return Err(Error::other_error(
                    "invalid `lightclient_lock_typeargs` conig".to_owned(),
                ));
            }
        }

        #[cfg(test)]
        let keybase = KeyRing::new(Store::Memory, "ckb", &config.id).map_err(Error::key_base)?;

        #[cfg(not(test))]
        let keybase =
            KeyRing::new(Default::default(), "ckb", &config.id).map_err(Error::key_base)?;

        // check out the existence of the secret key
        #[cfg(not(test))]
        let _: Secp256k1KeyPair = keybase.get_key(&config.key_name).map_err(Error::key_base)?;

        let ckb = CkbChain {
            rt,
            rpc_client,
            config,
            keybase,
            storage,
            cached_network: RwLock::new(None),
            cached_tx_assembler_address: RwLock::new(None),
            cached_onchain_packed_client: None,
        };
        ckb.print_status_log()?;

        Ok(ckb)
    }

    fn shutdown(self) -> Result<(), Error> {
        tracing::debug!("runtime of ckb chain endpoint shutdown");
        Ok(())
    }

    fn health_check(&self) -> Result<HealthCheck, Error> {
        Ok(HealthCheck::Healthy)
    }

    fn keybase(&self) -> &KeyRing<Self::SigningKeyPair> {
        &self.keybase
    }

    fn keybase_mut(&mut self) -> &mut KeyRing<Self::SigningKeyPair> {
        &mut self.keybase
    }

    fn get_signer(&self) -> Result<Signer, Error> {
        let key_entry = self
            .keybase()
            .get_key(&self.config.key_name)
            .map_err(Error::key_base)?;
        let signer = key_pair_to_signer(&key_entry)?;
        Ok(signer)
    }

    fn ibc_version(&self) -> Result<Option<Version>, Error> {
        Ok(None)
    }

    fn send_messages_and_wait_commit(
        &mut self,
        tracked_msgs: TrackedMsgs,
    ) -> Result<Vec<IbcEventWithHeight>, Error> {
        let updates = tracked_msgs
            .msgs
            .into_iter()
            .map(|msg| msg.try_into())
            .collect::<Result<Vec<EthClientState>, _>>()
            .map_err(|e| Error::send_tx(e.to_string()))?
            .into_iter()
            .map(|client| client.lightclient_update)
            .collect();

        match tracked_msgs.tracking_id {
            TrackingId::Static(NonCosmos::ETH_CREATE_CLIENT) => {
                self.create_eth_multi_client(updates)
            }
            TrackingId::Static(NonCosmos::ETH_UPDATE_CLIENT) => {
                self.update_eth_multi_client(updates)
            }
            _ => Err(Error::send_tx("unknown msg".to_owned())),
        }
    }

    fn send_messages_and_wait_check_tx(
        &mut self,
        _tracked_msgs: TrackedMsgs,
    ) -> Result<Vec<Response>, Error> {
        todo!()
    }

    fn verify_header(
        &mut self,
        _trusted: ICSHeight,
        _target: ICSHeight,
        _client_state: &AnyClientState,
    ) -> Result<Self::LightBlock, Error> {
        todo!()
    }

    fn check_misbehaviour(
        &mut self,
        _update: &UpdateClient,
        _client_state: &AnyClientState,
    ) -> Result<Option<MisbehaviourEvidence>, Error> {
        todo!()
    }

    fn query_balance(
        &self,
        _key_name: Option<&str>,
        _denom: Option<&str>,
    ) -> Result<Balance, Error> {
        todo!()
    }

    fn query_all_balances(&self, _key_name: Option<&str>) -> Result<Vec<Balance>, Error> {
        todo!()
    }

    fn query_denom_trace(&self, _hash: String) -> Result<DenomTrace, Error> {
        todo!()
    }

    fn query_commitment_prefix(&self) -> Result<CommitmentPrefix, Error> {
        todo!()
    }

    fn query_application_status(&self) -> Result<ChainStatus, Error> {
        todo!()
    }

    fn query_clients(
        &self,
        _request: QueryClientStatesRequest,
    ) -> Result<Vec<IdentifiedAnyClientState>, Error> {
        let mut clients = vec![];
        if self.cached_onchain_packed_client.is_some() {
            let client_state = IdentifiedAnyClientState {
                client_id: Default::default(),
                client_state: AnyClientState::Ckb(CkbClientState {
                    chain_id: self.id(),
                }),
            };
            clients.push(client_state);
        }
        Ok(clients)
    }

    fn query_client_state(
        &self,
        _request: QueryClientStateRequest,
        _include_proof: IncludeProof,
    ) -> Result<(AnyClientState, Option<MerkleProof>), Error> {
        todo!()
    }

    fn query_consensus_state(
        &self,
        _request: QueryConsensusStateRequest,
        _include_proof: IncludeProof,
    ) -> Result<(AnyConsensusState, Option<MerkleProof>), Error> {
        todo!()
    }

    fn query_consensus_state_heights(
        &self,
        _request: QueryConsensusStateHeightsRequest,
    ) -> Result<Vec<ICSHeight>, Error> {
        todo!()
    }

    fn query_upgraded_client_state(
        &self,
        _request: QueryUpgradedClientStateRequest,
    ) -> Result<(AnyClientState, MerkleProof), Error> {
        todo!()
    }

    fn query_upgraded_consensus_state(
        &self,
        _request: QueryUpgradedConsensusStateRequest,
    ) -> Result<(AnyConsensusState, MerkleProof), Error> {
        todo!()
    }

    fn query_connections(
        &self,
        _request: QueryConnectionsRequest,
    ) -> Result<Vec<IdentifiedConnectionEnd>, Error> {
        todo!()
    }

    fn query_client_connections(
        &self,
        _request: QueryClientConnectionsRequest,
    ) -> Result<Vec<ConnectionId>, Error> {
        todo!()
    }

    fn query_connection(
        &self,
        _request: QueryConnectionRequest,
        _include_proof: IncludeProof,
    ) -> Result<(ConnectionEnd, Option<MerkleProof>), Error> {
        todo!()
    }

    fn query_connection_channels(
        &self,
        _request: super::requests::QueryConnectionChannelsRequest,
    ) -> Result<Vec<IdentifiedChannelEnd>, Error> {
        todo!()
    }

    fn query_channels(
        &self,
        _request: QueryChannelsRequest,
    ) -> Result<Vec<IdentifiedChannelEnd>, Error> {
        todo!()
    }

    fn query_channel(
        &self,
        _request: QueryChannelRequest,
        _include_proof: IncludeProof,
    ) -> Result<(ChannelEnd, Option<MerkleProof>), Error> {
        todo!()
    }

    fn query_channel_client_state(
        &self,
        _request: QueryChannelClientStateRequest,
    ) -> Result<Option<IdentifiedAnyClientState>, Error> {
        todo!()
    }

    fn query_packet_commitment(
        &self,
        _request: super::requests::QueryPacketCommitmentRequest,
        _include_proof: IncludeProof,
    ) -> Result<(Vec<u8>, Option<MerkleProof>), Error> {
        todo!()
    }

    fn query_packet_commitments(
        &self,
        _request: QueryPacketCommitmentsRequest,
    ) -> Result<(Vec<Sequence>, ICSHeight), Error> {
        todo!()
    }

    fn query_packet_receipt(
        &self,
        _request: super::requests::QueryPacketReceiptRequest,
        _include_proof: IncludeProof,
    ) -> Result<(Vec<u8>, Option<MerkleProof>), Error> {
        todo!()
    }

    fn query_unreceived_packets(
        &self,
        _request: QueryUnreceivedPacketsRequest,
    ) -> Result<Vec<Sequence>, Error> {
        todo!()
    }

    fn query_packet_acknowledgement(
        &self,
        _request: QueryPacketAcknowledgementRequest,
        _include_proof: IncludeProof,
    ) -> Result<(Vec<u8>, Option<MerkleProof>), Error> {
        todo!()
    }

    fn query_packet_acknowledgements(
        &self,
        _request: QueryPacketAcknowledgementsRequest,
    ) -> Result<(Vec<Sequence>, ICSHeight), Error> {
        todo!()
    }

    fn query_unreceived_acknowledgements(
        &self,
        _request: QueryUnreceivedAcksRequest,
    ) -> Result<Vec<Sequence>, Error> {
        todo!()
    }

    fn query_next_sequence_receive(
        &self,
        _request: QueryNextSequenceReceiveRequest,
        _include_proof: IncludeProof,
    ) -> Result<(Sequence, Option<MerkleProof>), Error> {
        todo!()
    }

    fn query_txs(
        &self,
        _request: super::requests::QueryTxRequest,
    ) -> Result<Vec<IbcEventWithHeight>, Error> {
        todo!()
    }

    fn query_packet_events(
        &self,
        _request: super::requests::QueryPacketEventDataRequest,
    ) -> Result<Vec<IbcEventWithHeight>, Error> {
        todo!()
    }

    fn query_host_consensus_state(
        &self,
        _request: QueryHostConsensusStateRequest,
    ) -> Result<Self::ConsensusState, Error> {
        todo!()
    }

    fn build_client_state(
        &self,
        _height: ICSHeight,
        _settings: ClientSettings,
    ) -> Result<Self::ClientState, Error> {
        todo!()
    }

    fn build_consensus_state(
        &self,
        _light_block: Self::LightBlock,
    ) -> Result<Self::ConsensusState, Error> {
        todo!()
    }

    fn build_header(
        &mut self,
        _trusted_height: ICSHeight,
        _target_height: ICSHeight,
        _client_state: &AnyClientState,
    ) -> Result<(Self::Header, Vec<Self::Header>), Error> {
        todo!()
    }

    fn maybe_register_counterparty_payee(
        &mut self,
        _channel_id: &ChannelId,
        _port_id: &PortId,
        _counterparty_payee: &Signer,
    ) -> Result<(), Error> {
        todo!()
    }

    fn cross_chain_query(
        &self,
        _requests: Vec<CrossChainQueryRequest>,
    ) -> Result<Vec<CrossChainQueryResponse>, Error> {
        todo!()
    }

    fn subscribe(&mut self) -> Result<super::handle::Subscription, Error> {
        todo!()
    }

    fn query_incentivized_packet(
        &self,
        _: QueryIncentivizedPacketRequest,
    ) -> Result<QueryIncentivizedPacketResponse, Error> {
        todo!()
    }
}
