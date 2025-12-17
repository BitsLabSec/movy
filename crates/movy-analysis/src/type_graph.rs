use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Display,
};

use movy_types::abi::{
    MoveAbiSignatureToken, MoveFunctionAbi, MoveModuleAbi, MoveModuleId, MovePackageAbi,
};
use petgraph::{graph::NodeIndex, visit::EdgeRef};
use serde::{Deserialize, Serialize};
use serde_json_any_key::any_key_map;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TypeGraphNode {
    Function(MoveModuleId, MoveFunctionAbi),
    Type(MoveAbiSignatureToken), // Note: Must be dereferenced
}

impl Display for TypeGraphNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Function(m, v) => f.write_fmt(format_args!(
                "Function({:#}::{}::{})",
                m.module_address, m.module_name, v.name
            )),
            Self::Type(v) => f.write_fmt(format_args!("Type({:#})", v)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TypeGraphEdge {
    FunctionReturn,
    MutableReference,
    Reference,
    Value,
}

impl Display for TypeGraphEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FunctionReturn => f.write_str("r"),
            Self::MutableReference => f.write_str("&mut"),
            Self::Reference => f.write_str("&"),
            Self::Value => f.write_str("v"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveTypeGraph {
    graph: petgraph::Graph<TypeGraphNode, TypeGraphEdge>,
    #[serde(with = "any_key_map")]
    tys: BTreeMap<TypeGraphNode, NodeIndex>,
    modules: BTreeSet<MoveModuleId>,
}

impl Default for MoveTypeGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl MoveTypeGraph {
    pub fn new() -> Self {
        Self {
            graph: petgraph::Graph::new(),
            tys: BTreeMap::new(),
            modules: BTreeSet::new(),
        }
    }

    pub fn dot(&self) -> String {
        let dot = petgraph::dot::Dot::new(&self.graph);
        dot.to_string()
    }

    pub fn add_function(&mut self, function: &MoveFunctionAbi, module_id: &MoveModuleId) {
        let fnode = TypeGraphNode::Function(module_id.clone(), function.clone());
        let fidx = self.graph.add_node(fnode);

        for param in function.parameters.iter() {
            let (ty, edge) = match param {
                MoveAbiSignatureToken::Reference(v) => (*v.clone(), TypeGraphEdge::Reference),
                MoveAbiSignatureToken::MutableReference(v) => {
                    (*v.clone(), TypeGraphEdge::MutableReference)
                }
                _ => (param.clone(), TypeGraphEdge::Value),
            };

            let ty_node_idx = self.may_add_ty(ty);
            self.graph.add_edge(ty_node_idx, fidx, edge);
        }

        for rt in function.return_paramters.iter() {
            let ty = if let Some(deref) = rt.dereference() {
                *deref.clone()
            } else {
                rt.clone()
            };

            let node_idx = self.may_add_ty(ty);
            self.graph
                .add_edge(fidx, node_idx, TypeGraphEdge::FunctionReturn);
        }
    }

    pub fn add_module(&mut self, module: &MoveModuleAbi) {
        if self.modules.contains(&module.module_id) {
            log::trace!("{} has been analyzed", &module.module_id);
            return;
        }
        log::trace!("Analyze module {}", &module.module_id);
        self.modules.insert(module.module_id.clone());
        for func in module.functions.iter() {
            self.add_function(func, &module.module_id);
        }
    }

    pub fn add_package(&mut self, abi: &MovePackageAbi) {
        for module in abi.modules.iter() {
            self.add_module(module);
        }
    }

    pub fn find_consumers(
        &self,
        ty: &MoveAbiSignatureToken,
    ) -> Vec<(&MoveModuleId, &MoveFunctionAbi)> {
        let mut consumers = vec![];
        for (graph_ty, node) in self.tys.iter() {
            if let TypeGraphNode::Type(t) = graph_ty
                && t.partial_extract_ty_args(ty).is_some()
            {
                for edge in self
                    .graph
                    .edges_directed(*node, petgraph::Direction::Outgoing)
                {
                    if let TypeGraphNode::Function(m, f) = &self.graph[edge.target()] {
                        consumers.push((m, f));
                    }
                }
            }
        }
        consumers
    }

    pub fn find_producers(
        &self,
        ty: &MoveAbiSignatureToken,
    ) -> Vec<(MoveModuleId, MoveFunctionAbi)> {
        let mut producers = vec![];
        for (graph_ty, node) in self.tys.iter() {
            if let TypeGraphNode::Type(t) = graph_ty
                && t.partial_extract_ty_args(ty).is_some()
            {
                for edge in self
                    .graph
                    .edges_directed(*node, petgraph::Direction::Incoming)
                {
                    if let TypeGraphNode::Function(m, f) = &self.graph[edge.source()] {
                        producers.push((m.clone(), f.clone()));
                    }
                }
            }
        }
        producers
    }

    fn may_add_ty(&mut self, ty: MoveAbiSignatureToken) -> NodeIndex {
        let node = TypeGraphNode::Type(ty);
        if let Some(idx) = self.tys.get(&node) {
            *idx
        } else {
            let idx = self.graph.add_node(node.clone());
            self.tys.insert(node, idx);
            idx
        }
    }
}
