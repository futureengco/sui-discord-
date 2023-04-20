// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use move_binary_format::file_format::Visibility;
use move_binary_format::normalized::Type;
use move_core_types::language_storage::StructTag;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use sui_json_rpc_types::SuiTransactionBlockEffectsAPI;
use sui_move_build::BuildConfig;
use sui_protocol_config::ProtocolConfig;
use sui_types::base_types::{ObjectID, ObjectRef, SequenceNumber, SuiAddress};
use sui_types::gas_coin::GasCoin;
use sui_types::messages::{
    CallArg, ObjectArg, TransactionData, TransactionEffects, TEST_ONLY_GAS_UNIT_FOR_PUBLISH,
};
use sui_types::object::Owner;
use sui_types::{Identifier, SUI_FRAMEWORK_ADDRESS};
use test_utils::network::{TestCluster, TestClusterBuilder};
use tracing::{debug, error, info};

#[derive(Debug, Clone)]
struct EntryFunction {
    package: ObjectID,
    module: String,
    function: String,
    parameters: Vec<Type>,
}

enum InputObjectPassKind {
    ByValue,
    ByRef,
    ByMutRef,
}

struct SurferState {
    cluster: Arc<TestCluster>,
    address: SuiAddress,
    gas_object: ObjectRef,
    owned_objects: HashMap<StructTag, Vec<ObjectRef>>,
    immutable_objects: HashMap<StructTag, Vec<ObjectRef>>,
    /// Map from StructTag to a vector of shared objects, where each shared object is a tuple of
    /// (object ID, initial shared version).
    shared_objects: HashMap<StructTag, Vec<(ObjectID, SequenceNumber)>>,
    entry_functions: Arc<Vec<EntryFunction>>,
}

impl SurferState {
    pub async fn surf(mut self) {
        let mut rng = rand::thread_rng();
        let seed = rng.gen::<u64>();
        let mut rng = StdRng::seed_from_u64(seed);
        info!("Seed: {:?}", seed);
        let rgp = self.cluster.get_reference_gas_price().await;
        loop {
            let mut entry_functions = self.entry_functions.as_ref().clone();
            entry_functions.shuffle(&mut rng);
            for entry in entry_functions {
                let Some(args) = self.choose_function_call_args(entry.parameters, &mut rng) else {
                    continue;
                };
                let tx_data = TransactionData::new_move_call(
                    self.address,
                    entry.package,
                    Identifier::new(entry.module).unwrap(),
                    Identifier::new(entry.function).unwrap(),
                    vec![],
                    self.gas_object,
                    args,
                    TEST_ONLY_GAS_UNIT_FOR_PUBLISH * rgp,
                    rgp,
                )
                .unwrap();
                let tx = self.cluster.sign_transaction(&self.address, &tx_data);
                let effects = loop {
                    match self.cluster.execute_transaction(tx.clone()).await {
                        Ok(effects) => break effects,
                        Err(e) => {
                            error!("Error executing transaction: {:?}", e);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                };
                debug!(
                    "Successfully executed transaction {:?} with effects {:?}",
                    tx, effects
                );
            }
        }
    }

    fn process_effects(&mut self, effects: &TransactionEffects) {
        for (obj_ref, owner, write_kind) in effects.all_changed_objects() {}
    }

    pub fn matching_owned_objects_count(&self, type_tag: &StructTag) -> usize {
        self.owned_objects
            .get(type_tag)
            .map(|objects| objects.len())
            .unwrap_or(0)
    }

    pub fn matching_immutable_objects_count(&self, type_tag: &StructTag) -> usize {
        self.immutable_objects
            .get(type_tag)
            .map(|objects| objects.len())
            .unwrap_or(0)
    }

    pub fn matching_shared_objects_count(&self, type_tag: &StructTag) -> usize {
        self.shared_objects
            .get(type_tag)
            .map(|objects| objects.len())
            .unwrap_or(0)
    }

    pub fn choose_nth_owned_object(&mut self, type_tag: &StructTag, n: usize) -> ObjectRef {
        self.owned_objects.get_mut(type_tag).unwrap().swap_remove(n)
    }

    pub fn choose_nth_immutable_object(&self, type_tag: &StructTag, n: usize) -> ObjectRef {
        self.immutable_objects.get(type_tag).unwrap()[n]
    }

    pub fn choose_nth_shared_object(
        &self,
        type_tag: &StructTag,
        n: usize,
    ) -> (ObjectID, SequenceNumber) {
        self.shared_objects.get(type_tag).unwrap()[n]
    }

    pub fn choose_object_call_arg(
        &mut self,
        kind: InputObjectPassKind,
        arg_type: Type,
        chosen_owned_objects: &mut Vec<(StructTag, ObjectRef)>,
        rng: &mut StdRng,
    ) -> Option<CallArg> {
        let type_tag = match arg_type {
            Type::Struct {
                address,
                module,
                name,
                type_arguments,
            } => StructTag {
                address,
                module,
                name,
                type_params: type_arguments
                    .into_iter()
                    .map(|t| t.into_type_tag().unwrap())
                    .collect(),
            },
            _ => {
                return None;
            }
        };
        let owned = self.matching_owned_objects_count(&type_tag);
        let immutable = self.matching_immutable_objects_count(&type_tag);
        let shared = self.matching_shared_objects_count(&type_tag);

        let total_matching_count = match kind {
            InputObjectPassKind::ByValue => owned,
            InputObjectPassKind::ByRef => owned + immutable + shared,
            InputObjectPassKind::ByMutRef => owned + shared,
        };
        if total_matching_count == 0 {
            return None;
        }
        let mut n = rng.gen_range(0..total_matching_count);
        if n < owned {
            let obj_ref = self.choose_nth_owned_object(&type_tag, n);
            chosen_owned_objects.push((type_tag, obj_ref));
            return Some(CallArg::Object(ObjectArg::ImmOrOwnedObject(obj_ref)));
        }
        n -= owned;
        if matches!(kind, InputObjectPassKind::ByRef) {
            if n < immutable {
                let obj_ref = self.choose_nth_immutable_object(&type_tag, n);
                return Some(CallArg::Object(ObjectArg::ImmOrOwnedObject(obj_ref)));
            } else {
                n -= immutable;
            }
        }
        let (id, initial_shared_version) = self.choose_nth_shared_object(&type_tag, n);
        Some(CallArg::Object(ObjectArg::SharedObject {
            id,
            initial_shared_version,
            mutable: matches!(kind, InputObjectPassKind::ByMutRef),
        }))
    }

    fn choose_function_call_args(
        &mut self,
        params: Vec<Type>,
        rng: &mut StdRng,
    ) -> Option<Vec<CallArg>> {
        let mut args = vec![];
        let mut chosen_owned_objects = vec![];
        let mut failed = false;
        for param in params {
            let arg = match param {
                Type::Bool => CallArg::Pure(bcs::to_bytes(&rng.gen::<bool>()).unwrap()),
                Type::U8 => CallArg::Pure(bcs::to_bytes(&rng.gen::<u8>()).unwrap()),
                Type::U16 => CallArg::Pure(bcs::to_bytes(&rng.gen::<u16>()).unwrap()),
                Type::U32 => CallArg::Pure(bcs::to_bytes(&rng.gen::<u32>()).unwrap()),
                Type::U64 => CallArg::Pure(bcs::to_bytes(&rng.gen::<u64>()).unwrap()),
                Type::U128 => CallArg::Pure(bcs::to_bytes(&rng.gen::<u128>()).unwrap()),
                Type::Address => {
                    CallArg::Pure(bcs::to_bytes(&self.cluster.accounts.choose(rng)).unwrap())
                }
                ty @ Type::Struct { .. } => {
                    match self.choose_object_call_arg(
                        InputObjectPassKind::ByValue,
                        ty,
                        &mut chosen_owned_objects,
                        rng,
                    ) {
                        Some(arg) => arg,
                        None => {
                            failed = true;
                            break;
                        }
                    }
                }
                Type::Reference(ty) => {
                    match self.choose_object_call_arg(
                        InputObjectPassKind::ByRef,
                        *ty,
                        &mut chosen_owned_objects,
                        rng,
                    ) {
                        Some(arg) => arg,
                        None => {
                            failed = true;
                            break;
                        }
                    }
                }
                Type::MutableReference(ty) => {
                    match self.choose_object_call_arg(
                        InputObjectPassKind::ByMutRef,
                        *ty,
                        &mut chosen_owned_objects,
                        rng,
                    ) {
                        Some(arg) => arg,
                        None => {
                            failed = true;
                            break;
                        }
                    }
                }
                Type::U256 | Type::Signer | Type::Vector(_) | Type::TypeParameter(_) => {
                    failed = true;
                    break;
                }
            };
            args.push(arg);
        }
        if failed {
            for (struct_tag, obj_ref) in chosen_owned_objects {
                self.owned_objects
                    .get_mut(&struct_tag)
                    .unwrap()
                    .push(obj_ref);
            }
            None
        } else {
            Some(args)
        }
    }
}

pub async fn run() {
    let cluster = Arc::new(
        TestClusterBuilder::new()
            .with_num_validators(4)
            .with_epoch_duration_ms(20000)
            .build()
            .await
            .unwrap(),
    );
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.extend(["tests", "move_building_blocks"]);
    let entry_functions = publish_package(cluster.as_ref(), path).await;
    let account = cluster.accounts[0];
    let mut gas_objects: Vec<_> = cluster
        .wallet
        .gas_objects(account)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, o)| o.object_ref())
        .collect();
    let gas_object = gas_objects.pop().unwrap();
    let owned_objects = HashMap::from([(GasCoin::type_(), gas_objects)]);
    let state = SurferState {
        cluster: cluster.clone(),
        address: account,
        gas_object,
        owned_objects,
        immutable_objects: HashMap::new(),
        shared_objects: HashMap::new(),
        entry_functions: Arc::new(entry_functions),
    };
    state.surf().await;
}

async fn publish_package(cluster: &TestCluster, path: PathBuf) -> Vec<EntryFunction> {
    let sender = cluster.accounts[0];
    let rgp = cluster.get_reference_gas_price().await;
    let gas_payment = cluster
        .wallet
        .gas_for_owner_budget(
            sender,
            TEST_ONLY_GAS_UNIT_FOR_PUBLISH * rgp,
            BTreeSet::new(),
        )
        .await
        .unwrap()
        .1
        .object_ref();
    let package = BuildConfig::new_for_testing().build(path).unwrap();
    let modules = package.get_package_bytes(false);
    let tx_data = TransactionData::new_module(
        sender,
        gas_payment,
        modules,
        package.dependency_ids.published.values().cloned().collect(),
        TEST_ONLY_GAS_UNIT_FOR_PUBLISH * rgp,
        rgp,
    );
    let tx = cluster.sign_transaction(&sender, &tx_data);
    let response = loop {
        match cluster.execute_transaction(tx.clone()).await {
            Ok(response) => {
                break response;
            }
            Err(err) => {
                error!("Failed to publish package: {:?}", err);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };
    let package_id = response
        .effects
        .unwrap()
        .created()
        .iter()
        .find(|o| o.owner == Owner::Immutable)
        .unwrap()
        .object_id();
    let package = loop {
        if let Some(package) = cluster
            .fullnode_handle
            .sui_node
            .state()
            .get_object(&package_id)
            .await
            .unwrap()
        {
            break package;
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    info!("Successfully published package at ID: {:?}", package_id);
    let move_package = package.data.try_into_package().unwrap();
    let move_binary_format_version =
        ProtocolConfig::get_for_max_version().move_binary_format_version();
    let entry_functions: Vec<_> = move_package
        .normalize(move_binary_format_version)
        .unwrap()
        .into_iter()
        .map(|(module_name, module)| {
            module
                .functions
                .into_iter()
                .filter_map(|(func_name, func)| {
                    if !func.is_entry || !matches!(func.visibility, Visibility::Public) {
                        return None;
                    }
                    if !func.type_parameters.is_empty() {
                        return None;
                    }
                    let mut parameters = func.parameters;
                    if let Some(last_param) = parameters.last().as_ref() {
                        if is_type_tx_context(last_param) {
                            parameters.pop();
                        }
                    }
                    Some(EntryFunction {
                        package: package_id,
                        module: module_name.clone(),
                        function: func_name.to_string(),
                        parameters,
                    })
                })
                .collect::<Vec<_>>()
        })
        .flatten()
        .collect();
    info!(
        "Number of entry functions discovered: {:?}",
        entry_functions.len()
    );
    debug!("Entry functions: {:?}", entry_functions);
    entry_functions
}

fn is_type_tx_context(ty: &Type) -> bool {
    match ty {
        Type::Reference(inner) | Type::MutableReference(inner) => match inner.as_ref() {
            Type::Struct {
                address,
                module,
                name,
                type_arguments,
            } => {
                address == &SUI_FRAMEWORK_ADDRESS
                    && module == &Identifier::new("tx_context").unwrap()
                    && name == &Identifier::new("TxContext").unwrap()
                    && type_arguments.is_empty()
            }
            _ => false,
        },
        _ => false,
    }
}
