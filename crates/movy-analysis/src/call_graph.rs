use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Display,
};

use itertools::Itertools;
use movy_types::{
    abi::{MoveAbiSignatureToken, MoveModuleId},
    bytecode::MoveModuleBytecodeAnalysis,
};
use petgraph::graph::NodeIndex;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoveCallGraphNode {
    module_id: MoveModuleId,
    function: String,
}

impl Display for MoveCallGraphNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{:#}:{}:{}",
            self.module_id.module_address, self.module_id.module_name, self.function
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoveCallGraphEdge {
    type_parameters: Vec<MoveAbiSignatureToken>,
}

impl Display for MoveCallGraphEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            &self
                .type_parameters
                .iter()
                .map(|v| format!("{:#}", v))
                .join(", "),
        )
    }
}

#[derive(Debug, Clone)]
pub struct MoveCallGraph {
    graph: petgraph::Graph<MoveCallGraphNode, MoveCallGraphEdge>,
    functions: BTreeMap<MoveCallGraphNode, NodeIndex>,
    modules: BTreeSet<MoveModuleId>,
}

impl Default for MoveCallGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl MoveCallGraph {
    pub fn new() -> Self {
        Self {
            graph: petgraph::Graph::new(),
            functions: BTreeMap::new(),
            modules: BTreeSet::new(),
        }
    }

    pub fn dot(&self) -> String {
        let dot = petgraph::dot::Dot::new(&self.graph);
        dot.to_string()
    }

    pub fn add_bytecode_analysis(&mut self, result: &MoveModuleBytecodeAnalysis) {
        if self.modules.contains(&result.abi.module_id) {
            return;
        }
        self.modules.insert(result.abi.module_id.clone());

        for (caller, calls) in result.calls.iter() {
            let caller = MoveCallGraphNode {
                module_id: result.abi.module_id.clone(),
                function: caller.name.clone(),
            };
            let src = self.may_add_function(caller);
            for call in calls.iter().unique() {
                let dst = MoveCallGraphNode {
                    module_id: call.module.clone(),
                    function: call.abi.name.clone(),
                };
                let dst = self.may_add_function(dst);

                let edge = MoveCallGraphEdge {
                    type_parameters: call.tys.clone(),
                };
                self.graph.add_edge(src, dst, edge);
            }
        }
    }

    fn may_add_function(&mut self, fcall: MoveCallGraphNode) -> NodeIndex {
        if let Some(idx) = self.functions.get(&fcall) {
            *idx
        } else {
            let idx = self.graph.add_node(fcall.clone());
            self.functions.insert(fcall, idx);
            idx
        }
    }
}
