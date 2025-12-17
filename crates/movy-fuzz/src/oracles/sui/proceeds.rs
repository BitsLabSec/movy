use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Neg,
};

use log::debug;
use move_trace_format::format::TraceEvent;
use move_vm_stack::Stack;

use movy_replay::tracer::{concolic::ConcolicState, oracle::SuiGeneralOracle};
use movy_types::{
    error::MovyError,
    input::{InputArgument, MoveSequence, SuiObjectInputArgument},
    oracle::OracleFinding,
};
use serde_json::json;
use sui_json_rpc_types::BalanceChange;
use sui_types::{
    TypeTag,
    base_types::{ObjectID, SequenceNumber},
    coin::Coin,
    digests::ObjectDigest,
    effects::{TransactionEffects, TransactionEffectsAPI},
    execution_status::ExecutionStatus,
    gas_coin::GAS,
    object::Owner,
    storage::ObjectStore,
    transaction::{InputObjectKind, SharedObjectMutability},
};

use crate::{
    meta::HasFuzzMetadata,
    state::{ExtraNonSerdeFuzzState, HasExtraState},
};

#[derive(Debug, Default, Clone)]
pub struct ProceedsOracle {
    input_objects: Vec<InputObjectKind>,
}

pub fn get_balance_changes_from_effect<P: ObjectStore>(
    object_provider: &P,
    effects: &TransactionEffects,
    input_objs: Vec<InputObjectKind>,
    mocked_coin: ObjectID,
) -> Option<Vec<BalanceChange>> {
    let (_, gas_owner) = effects.gas_object();

    // Only charge gas when tx fails, skip all object parsing
    if effects.status() != &ExecutionStatus::Success {
        return Some(vec![BalanceChange {
            owner: gas_owner,
            coin_type: GAS::type_tag(),
            amount: effects.gas_cost_summary().net_gas_usage().neg() as i128,
        }]);
    }

    let all_mutated = effects
        .all_changed_objects()
        .into_iter()
        .filter_map(|((id, version, digest), _, _)| {
            if id == mocked_coin {
                return None;
            }
            Some((id, version, Some(digest)))
        })
        .collect::<Vec<_>>();

    let input_objs_to_digest = input_objs
        .iter()
        .filter_map(|k| match k {
            InputObjectKind::ImmOrOwnedMoveObject(o) => Some((o.0, o.2)),
            InputObjectKind::MovePackage(_) | InputObjectKind::SharedMoveObject { .. } => None,
        })
        .collect::<HashMap<ObjectID, ObjectDigest>>();
    let unwrapped_then_deleted = effects
        .unwrapped_then_deleted()
        .iter()
        .map(|e| e.0)
        .collect::<HashSet<_>>();
    get_balance_changes(
        object_provider,
        &effects
            .modified_at_versions()
            .into_iter()
            .filter_map(|(id, version)| {
                if id == mocked_coin {
                    return None;
                }
                // We won't be able to get dynamic object from object provider today
                if unwrapped_then_deleted.contains(&id) {
                    return None;
                }
                Some((id, version, input_objs_to_digest.get(&id).cloned()))
            })
            .collect::<Vec<_>>(),
        &all_mutated,
    )
}

pub fn get_balance_changes<P: ObjectStore>(
    object_provider: &P,
    modified_at_version: &[(ObjectID, SequenceNumber, Option<ObjectDigest>)],
    all_mutated: &[(ObjectID, SequenceNumber, Option<ObjectDigest>)],
) -> Option<Vec<BalanceChange>> {
    // 1. subtract all input coins
    let balances = fetch_coins(object_provider, modified_at_version)?
        .into_iter()
        .fold(
            BTreeMap::<_, i128>::new(),
            |mut acc, (owner, type_, amount)| {
                *acc.entry((owner, type_)).or_default() -= amount as i128;
                acc
            },
        );
    // 2. add all mutated coins
    let balances = fetch_coins(object_provider, all_mutated)?.into_iter().fold(
        balances,
        |mut acc, (owner, type_, amount)| {
            *acc.entry((owner, type_)).or_default() += amount as i128;
            acc
        },
    );

    Some(
        balances
            .into_iter()
            .filter_map(|((owner, coin_type), amount)| {
                if amount == 0 {
                    return None;
                }
                Some(BalanceChange {
                    owner,
                    coin_type,
                    amount,
                })
            })
            .collect(),
    )
}

fn fetch_coins<P: ObjectStore>(
    object_provider: &P,
    objects: &[(ObjectID, SequenceNumber, Option<ObjectDigest>)],
) -> Option<Vec<(Owner, TypeTag, u64)>> {
    let mut all_mutated_coins = vec![];
    for (id, version, digest_opt) in objects {
        // TODO: use multi get object
        let o = object_provider.get_object_by_key(id, *version)?;
        if let Some(type_) = o.type_()
            && type_.is_coin()
        {
            if let Some(digest) = digest_opt {
                // TODO: can we return Err here instead?
                assert_eq!(
                    *digest,
                    o.digest(),
                    "Object digest mismatch--got bad data from object_provider?"
                )
            }
            let [coin_type]: [TypeTag; 1] = type_.clone().into_type_params().try_into().unwrap();
            all_mutated_coins.push((
                o.owner.clone(),
                coin_type,
                // we know this is a coin, safe to unwrap
                Coin::extract_balance_if_coin(&o).unwrap().unwrap().1,
            ))
        }
    }
    Some(all_mutated_coins)
}

impl<T, S, E> SuiGeneralOracle<T, S> for ProceedsOracle
where
    S: HasExtraState<ExtraState = ExtraNonSerdeFuzzState<E>> + HasFuzzMetadata,
    T: ObjectStore,
{
    fn pre_execution(
        &mut self,
        _db: &T,
        _state: &mut S,
        sequence: &MoveSequence,
    ) -> Result<(), MovyError> {
        self.input_objects = sequence
            .inputs
            .iter()
            .filter_map(|input| {
                if let InputArgument::Object(_ty, obj) = input {
                    match obj {
                        SuiObjectInputArgument::ImmOrOwnedObject(obj_ref) => {
                            Some(InputObjectKind::ImmOrOwnedMoveObject(*obj_ref))
                        }
                        SuiObjectInputArgument::SharedObject {
                            id,
                            initial_shared_version,
                            mutable,
                        } => Some(InputObjectKind::SharedMoveObject {
                            id: *id,
                            initial_shared_version: *initial_shared_version,
                            mutability: if *mutable {
                                SharedObjectMutability::Mutable
                            } else {
                                SharedObjectMutability::Immutable
                            },
                        }),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .collect();
        Ok(())
    }

    fn event(
        &mut self,
        _event: &TraceEvent,
        _stack: Option<&Stack>,
        _symbol_stack: &ConcolicState,
        _current_function: Option<movy_types::input::FunctionIdent>,
        _state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        Ok(vec![])
    }

    fn done_execution(
        &mut self,
        db: &T,
        state: &mut S,
        effects: &TransactionEffects,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        if !state
            .extra_state()
            .global_outcome
            .as_ref()
            .is_some_and(|o| o.exec.allowed_success)
        {
            return Ok(Vec::new());
        }
        let balance_change = get_balance_changes_from_effect(
            db,
            effects,
            self.input_objects.clone(),
            state.fuzz_state().gas_id.into(),
        );
        debug!("gas id: {:?}", state.fuzz_state().gas_id);
        match balance_change {
            Some(bc) => {
                debug!("Balance change: {:?}", bc);
                if bc.iter().all(|c| c.amount >= 0) && bc.iter().any(|c| c.amount > 0) {
                    debug!("Found proceeds: {:?}", bc);
                    let finding = OracleFinding {
                        oracle: "ProceedsOracle".to_string(),
                        severity: movy_types::oracle::Severity::Critical,
                        extra: json!({
                            "message": "Positive proceeds detected",
                            "balance_changes": bc,
                        }),
                    };
                    return Ok(vec![finding]);
                }
            }
            None => {
                debug!("Failed to get balance change");
            }
        }
        Ok(vec![])
    }
}
