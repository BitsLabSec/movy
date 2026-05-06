use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use knowdit_repo_model::cg::{
    CallGraph, Contract, FileChunk, FileLocation, Function, FunctionCall, Interface,
};
use movy_sui::compile::{SuiCompiledPackage, resolve_local_dependency_paths};
use movy_types::{
    abi::{MoveAbiSignatureToken, MoveFunctionAbi},
    bytecode::MoveModuleBytecodeAnalysis,
    error::MovyError,
};

pub fn load_call_graph(package_root: &Path, test_mode: bool) -> Result<CallGraph, MovyError> {
    let package_root = canonical_package_root(package_root)?;
    let search_roots = resolve_search_roots(&package_root, test_mode)?;
    let placeholder_loc = placeholder_location();

    let mut next_contract_id = 1;
    let mut next_function_id = 1;
    let mut next_call_id = 1;
    let mut contracts = BTreeMap::new();
    let mut seen_modules = BTreeSet::new();
    let mut module_ids = BTreeMap::<String, i32>::new();
    let mut function_ids = BTreeMap::<MoveFunctionKey, i32>::new();
    let mut function_locations = BTreeMap::<i32, (i32, usize)>::new();
    let mut pending_calls = BTreeMap::<MoveFunctionKey, BTreeSet<PendingCall>>::new();

    for root in &search_roots {
        let package = SuiCompiledPackage::build_checked(root, test_mode, false, true)?;
        for module in package.all_modules_iter() {
            if is_sui_std_module(module.address().to_string().as_str()) {
                continue;
            }

            let module_id = canonical_module_id(
                module.address().to_string().as_str(),
                module.name().as_str(),
            );
            if !seen_modules.insert(module_id.clone()) {
                continue;
            }

            let contract_id = next_contract_id;
            next_contract_id += 1;
            module_ids.insert(module_id.clone(), contract_id);

            let relative_file_path = infer_relative_source_path(
                &package_root,
                root,
                &package.package_name,
                module.name().as_str(),
            );
            let mut functions = Vec::new();
            let analysis = MoveModuleBytecodeAnalysis::from_sui_module(module);

            for (function, calls) in analysis.calls {
                let function_id = next_function_id;
                next_function_id += 1;
                let function_index = functions.len();
                let function_key = MoveFunctionKey {
                    module_id: module_id.clone(),
                    name: function.name.clone(),
                };
                function_ids.insert(function_key.clone(), function_id);
                function_locations.insert(function_id, (contract_id, function_index));

                let call_targets = pending_calls.entry(function_key).or_default();
                for call in calls {
                    call_targets.insert(PendingCall {
                        callee: MoveFunctionKey {
                            module_id: canonical_module_id(
                                call.module.module_address.to_string().as_str(),
                                &call.module.module_name,
                            ),
                            name: call.abi.name.clone(),
                        },
                        args: render_args(&call.abi),
                        description: render_call_description(&call.tys),
                    });
                }

                let args = render_args(&function);
                let returns = render_returns(&function);
                let return_suffix = if returns.is_empty() {
                    String::new()
                } else {
                    format!(": {}", returns)
                };
                functions.push(Function {
                    id: function_id,
                    name: function.name.clone(),
                    args: args.clone(),
                    relative_file_path: relative_file_path.clone(),
                    loc: placeholder_loc,
                    content: None,
                    calls: Vec::new(),
                    description: Some(format!(
                        "{} {}({}){}",
                        function.visibility, function.name, args, return_suffix
                    )),
                });
            }

            contracts.insert(
                contract_id,
                Contract {
                    id: contract_id,
                    name: module_id,
                    relative_file_path,
                    chunk: FileChunk {
                        loc: placeholder_loc,
                        content: String::new(),
                    },
                    functions,
                    description: None,
                },
            );
        }
    }

    for call_set in pending_calls.values() {
        for pending_call in call_set {
            if function_ids.contains_key(&pending_call.callee) {
                continue;
            }

            let contract_id = if let Some(existing_id) =
                module_ids.get(&pending_call.callee.module_id)
            {
                *existing_id
            } else {
                let contract_id = next_contract_id;
                next_contract_id += 1;
                module_ids.insert(pending_call.callee.module_id.clone(), contract_id);
                contracts.insert(
                    contract_id,
                    Contract {
                        id: contract_id,
                        name: pending_call.callee.module_id.clone(),
                        relative_file_path: synthetic_relative_path(&pending_call.callee.module_id),
                        chunk: FileChunk {
                            loc: placeholder_loc,
                            content: String::new(),
                        },
                        functions: Vec::new(),
                        description: Some(
                            "external module referenced from Move call graph".to_string(),
                        ),
                    },
                );
                contract_id
            };

            let function_id = next_function_id;
            next_function_id += 1;
            let contract = contracts
                .get_mut(&contract_id)
                .expect("callee contract should exist");
            let function_index = contract.functions.len();
            contract.functions.push(Function {
                id: function_id,
                name: pending_call.callee.name.clone(),
                args: pending_call.args.clone(),
                relative_file_path: contract.relative_file_path.clone(),
                loc: placeholder_loc,
                content: None,
                calls: Vec::new(),
                description: Some("external function referenced from Move bytecode".to_string()),
            });
            function_ids.insert(pending_call.callee.clone(), function_id);
            function_locations.insert(function_id, (contract_id, function_index));
        }
    }

    for (caller_key, call_set) in pending_calls {
        let caller_id = *function_ids
            .get(&caller_key)
            .expect("caller function id should exist");
        let (contract_id, function_index) = *function_locations
            .get(&caller_id)
            .expect("caller function location should exist");
        let function = &mut contracts
            .get_mut(&contract_id)
            .expect("caller contract should exist")
            .functions[function_index];

        for pending_call in call_set {
            function.calls.push(FunctionCall {
                id: next_call_id,
                from_id: caller_id,
                to_id: *function_ids
                    .get(&pending_call.callee)
                    .expect("callee function id should exist"),
                description: pending_call.description,
            });
            next_call_id += 1;
        }
    }

    Ok(CallGraph {
        contracts,
        interfaces: BTreeMap::<i32, Interface>::new(),
    })
}

fn canonical_package_root(package_root: &Path) -> Result<PathBuf, MovyError> {
    let package_root = package_root
        .canonicalize()
        .unwrap_or_else(|_| package_root.to_path_buf());
    if !package_root.is_dir() {
        return Err(MovyError::Any(anyhow!(
            "Move package root {} is not a directory",
            package_root.display()
        )));
    }
    if !package_root.join("Move.toml").is_file() {
        return Err(MovyError::Any(anyhow!(
            "Move package root {} does not contain Move.toml",
            package_root.display()
        )));
    }
    Ok(package_root)
}

fn resolve_search_roots(package_root: &Path, test_mode: bool) -> Result<Vec<PathBuf>, MovyError> {
    let mut roots = vec![package_root.to_path_buf()];
    for root in resolve_local_dependency_paths(&[package_root.to_path_buf()], test_mode)? {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    Ok(roots)
}

fn render_call_description(type_arguments: &[MoveAbiSignatureToken]) -> Option<String> {
    if type_arguments.is_empty() {
        None
    } else {
        Some(format!(
            "type_args=<{}>",
            type_arguments
                .iter()
                .map(|ty| format!("{:#}", ty))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

fn render_args(function: &MoveFunctionAbi) -> String {
    function
        .parameters
        .iter()
        .map(|parameter| format!("{:#}", parameter))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_returns(function: &MoveFunctionAbi) -> String {
    function
        .return_paramters
        .iter()
        .map(|parameter| format!("{:#}", parameter))
        .collect::<Vec<_>>()
        .join(", ")
}

fn infer_relative_source_path(
    analysis_root: &Path,
    package_root: &Path,
    package_name: &str,
    module_name: &str,
) -> PathBuf {
    let preferred_roots = [
        package_root.join("sources"),
        package_root.join("tests"),
        package_root
            .join("build")
            .join(package_name)
            .join("sources"),
    ];

    for root in preferred_roots {
        if let Some(source_path) = find_module_source_file(&root, module_name) {
            return relativize_to_analysis_root(
                analysis_root,
                package_root,
                package_name,
                &source_path,
            );
        }
    }

    PathBuf::from(package_name)
        .join("sources")
        .join(format!("{module_name}.move"))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MoveFunctionKey {
    module_id: String,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PendingCall {
    callee: MoveFunctionKey,
    args: String,
    description: Option<String>,
}

fn placeholder_location() -> FileLocation {
    FileLocation {
        start_line: 1,
        start_column: 0,
        end_line: 1,
        end_column: 0,
    }
}

fn synthetic_relative_path(module_id: &str) -> PathBuf {
    let (address, module_name) = module_id
        .split_once("::")
        .unwrap_or(("external", module_id));
    PathBuf::from(format!(
        "external/{}_{}.move",
        sanitize_path_component(address),
        sanitize_path_component(module_name)
    ))
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            ':' | '/' | '\\' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect()
}

fn find_module_source_file(root: &Path, module_name: &str) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }

    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if file_type.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("move")
                && path.file_stem().and_then(|stem| stem.to_str()) == Some(module_name)
            {
                return Some(path);
            }
        }
    }

    None
}

fn relativize_to_analysis_root(
    analysis_root: &Path,
    package_root: &Path,
    package_name: &str,
    source_path: &Path,
) -> PathBuf {
    let absolute = source_path
        .canonicalize()
        .unwrap_or_else(|_| source_path.to_path_buf());
    if let Ok(relative) = absolute.strip_prefix(analysis_root) {
        return relative.to_path_buf();
    }
    if let Ok(relative) = absolute.strip_prefix(package_root) {
        return PathBuf::from(package_name).join(relative);
    }
    PathBuf::from(package_name).join(
        absolute
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("unknown.move")),
    )
}

fn canonical_module_id(address: &str, module_name: &str) -> String {
    format!("{}::{}", canonicalize_address(address), module_name)
}

fn canonicalize_address(value: &str) -> String {
    let trimmed = value.trim();
    let trimmed = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    let trimmed = trimmed.trim_start_matches('0');
    let normalized = if trimmed.is_empty() { "0" } else { trimmed };
    format!("0x{}", normalized.to_ascii_lowercase())
}

fn is_sui_std_module(address: &str) -> bool {
    matches!(
        canonicalize_address(address).as_str(),
        "0x1" | "0x2" | "0x3" | "0xd"
    )
}
