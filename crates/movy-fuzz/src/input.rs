use std::hash::Hash;
use std::io::Read;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Display,
};

use libafl::inputs::Input;
use libafl_bolts::fs::write_file_atomic;
use libafl_bolts::generic_hash_std;
use movy_replay::tracer::op::{CmpOp, Log, Magic};
use movy_types::input::{FunctionIdent, MoveAddress, MoveSequence};
use serde::{Deserialize, Serialize};

use crate::executor::{ExecutionExtraOutcome, ExecutionOutcome};
use crate::flash::FlashWrapper;
use crate::meta::FuzzMetadata;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveFuzzInput {
    pub sequence: MoveSequence,

    // Input Metadata
    pub outcome: Option<ExecutionOutcome>,
    pub magic_number_pool: BTreeMap<String, BTreeMap<String, BTreeMap<String, BTreeSet<Vec<u8>>>>>,
    pub flash: Option<FlashWrapper>,
    pub display: Option<String>,
}

impl Display for MoveFuzzInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.format_impl())
    }
}

impl Hash for MoveFuzzInput {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.sequence.hash(state);
        // self.flash.hash(state);
        // ignore metadata
    }
}

impl Default for MoveFuzzInput {
    fn default() -> Self {
        Self {
            sequence: MoveSequence {
                commands: vec![],
                inputs: vec![],
            },
            outcome: None,
            magic_number_pool: BTreeMap::new(),
            flash: None,
            display: None,
        }
    }
}

impl MoveFuzzInput {
    pub fn format(&self) -> String {
        self.format_impl()
    }

    pub fn format_impl(&self) -> String {
        let flash = if let Some(flash) = &self.flash {
            format!("{}", flash)
        } else {
            "No flash".to_string()
        };

        let outcome = if let Some(oc) = &self.outcome {
            format!("{}", oc)
        } else {
            "No outcome".to_string()
        };
        format!(
            "{}\nFlash: |\n{}\nOutcome: |\n{}",
            self.sequence, flash, outcome
        )
    }

    pub fn new() -> Self {
        Self {
            ..Default::default()
        }
    }

    // pub fn flash(flash: FlashWrapper) -> Result<Self, MovyError> {
    //     let ptb = flash.flash_seed(&flash.flash_coin, flash.initial_flash_amount)?;

    //     Ok(Self {
    //         sequence: ptb,
    //         flash: Some(flash),
    //         ..Default::default()
    //     })
    // }
}

pub trait MoveInput: Display {
    fn sequence(&self) -> &MoveSequence;

    fn sequence_mut(&mut self) -> &mut MoveSequence;

    fn outcome(&self) -> &Option<ExecutionOutcome>;
    fn outcome_mut(&mut self) -> &mut Option<ExecutionOutcome>;

    fn update_magic_number(&mut self, outcome: &ExecutionExtraOutcome);
    fn magic_number_pool(&self, meta: &FuzzMetadata) -> BTreeMap<FunctionIdent, BTreeSet<Vec<u8>>>;
    // fn flash(&self) -> &Option<FlashWrapper>;

    fn display_mut(&mut self) -> &mut Option<String>;
    fn format(&self) -> String;
}

impl MoveInput for MoveFuzzInput {
    fn sequence(&self) -> &MoveSequence {
        &self.sequence
    }

    fn sequence_mut(&mut self) -> &mut MoveSequence {
        &mut self.sequence
    }

    fn outcome(&self) -> &Option<ExecutionOutcome> {
        &self.outcome
    }

    fn outcome_mut(&mut self) -> &mut Option<ExecutionOutcome> {
        &mut self.outcome
    }

    fn update_magic_number(&mut self, outcome: &ExecutionExtraOutcome) {
        self.magic_number_pool.clear(); // clear old pool, and rebuild it
        let mutate_cmps = outcome.logs.clone();
        for (func, cmps) in &mutate_cmps {
            let pkg = &func.0.module_address;
            let module = &func.0.module_name;
            let func = &func.1;
            let function_pool = self
                .magic_number_pool
                .entry(pkg.to_string())
                .or_default()
                .entry(module.to_string())
                .or_default()
                .entry(func.to_string())
                .or_default();
            for cmp in cmps.iter() {
                let Log::CmpLog(cmp) = cmp else {
                    continue;
                };
                if matches!(cmp.op, CmpOp::EQ | CmpOp::GE | CmpOp::LE) {
                    for magic in [&cmp.lhs, &cmp.rhs] {
                        match magic {
                            Magic::U8(v) => {
                                function_pool.insert(vec![*v]);
                            }
                            Magic::U16(v) => {
                                function_pool.insert(v.to_le_bytes().to_vec());
                            }
                            Magic::U32(v) => {
                                function_pool.insert(v.to_le_bytes().to_vec());
                            }
                            Magic::U64(v) => {
                                function_pool.insert(v.to_le_bytes().to_vec());
                            }
                            Magic::U128(v) => {
                                function_pool.insert(v.to_le_bytes().to_vec());
                            }
                            Magic::U256(v) => {
                                function_pool.insert(v.to_le_bytes::<32>().to_vec());
                            }
                            Magic::Bytes(v) => {
                                if !v.is_empty() {
                                    function_pool.insert(v.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn magic_number_pool(&self, meta: &FuzzMetadata) -> BTreeMap<FunctionIdent, BTreeSet<Vec<u8>>> {
        let mut res = BTreeMap::new();
        for (pkg, module_map) in &self.magic_number_pool {
            let pkg = MoveAddress::from_str(pkg).unwrap();
            let pkg = meta.module_address_to_package.get(&pkg).unwrap_or(&pkg);
            for (module, func_map) in module_map {
                for (func, cmps) in func_map {
                    let function_ident = FunctionIdent::new(pkg, module, func);
                    res.insert(function_ident, cmps.clone());
                }
            }
        }
        res
    }

    // fn flash(&self) -> &Option<FlashWrapper> {
    //     &self.flash
    // }

    fn display_mut(&mut self) -> &mut Option<String> {
        &mut self.display
    }
    fn format(&self) -> String {
        self.format_impl()
    }
}

impl Input for MoveFuzzInput {
    fn to_file<P>(&self, path: P) -> Result<(), libafl::Error>
    where
        P: AsRef<std::path::Path>,
    {
        write_file_atomic(
            path,
            &serde_json::to_vec(self).map_err(|e| libafl::Error::serialize(e.to_string()))?,
        )
    }

    fn from_file<P>(path: P) -> Result<Self, libafl::Error>
    where
        P: AsRef<std::path::Path>,
    {
        let mut file = std::fs::File::open(path)?;
        let mut bytes = vec![];
        file.read_to_end(&mut bytes)?;
        serde_json::from_slice(&bytes).map_err(|e| libafl::Error::serialize(e.to_string()))
    }

    fn generate_name(&self, id: Option<libafl::corpus::CorpusId>) -> String {
        if let Some(id) = id {
            format!("{}.json", id)
        } else {
            format!("{:016x}.json", generic_hash_std(self))
        }
    }
}
