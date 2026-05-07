use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    path::{Path, PathBuf},
};

use color_eyre::eyre::eyre;
use move_binary_format::file_format::FunctionDefinitionIndex;
use move_bytecode_source_map::source_map::SourceMap;
use move_compiler::{compiled_unit::CompiledUnit, shared::files::MappedFiles};
use move_core_types::language_storage::ModuleId;
use movy_types::{error::MovyError, input::MoveAddress};
use serde::{Deserialize, Serialize};
use sui_types::base_types::ObjectID;

use crate::compile::compile_package_artifacts;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BytecodeLocation {
    pub module: ModuleId,
    pub function: u16,
    pub pc: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineCoverageMap {
    files: BTreeMap<PathBuf, FileCoverageMap>,
    function_to_source: BTreeMap<FunctionLocation, FunctionSource>,
    pc_to_line: BTreeMap<BytecodeLocation, SourceLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileCoverageMap {
    functions: BTreeMap<String, usize>,
    lines: BTreeSet<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct SourceLine {
    file: PathBuf,
    line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct FunctionSource {
    file: PathBuf,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct FunctionLocation {
    module: ModuleId,
    function: u16,
}

impl LineCoverageMap {
    pub fn for_locals(
        locals: &[PathBuf],
        test_mode: bool,
    ) -> Result<Option<LineCoverageMap>, MovyError> {
        Self::for_locals_with_package_ids(locals, test_mode, &[])
    }

    pub fn for_locals_with_package_ids(
        locals: &[PathBuf],
        test_mode: bool,
        package_ids: &[MoveAddress],
    ) -> Result<Option<LineCoverageMap>, MovyError> {
        if locals.is_empty() {
            return Ok(None);
        }

        let mut out = LineCoverageMap {
            files: BTreeMap::new(),
            function_to_source: BTreeMap::new(),
            pc_to_line: BTreeMap::new(),
        };

        for (idx, local) in locals.iter().enumerate() {
            let local = std::fs::canonicalize(local)?;
            let artifacts = compile_package_artifacts(&local, test_mode)?;
            let package_id: MoveAddress = package_ids
                .get(idx)
                .copied()
                .unwrap_or_else(|| artifacts.published_at.unwrap_or(ObjectID::ZERO).into());
            for unit in artifacts.package.root_compiled_units.iter() {
                out.add_unit(
                    &unit.unit,
                    &unit.source_path,
                    &artifacts.package.file_map,
                    package_id,
                )?;
            }
        }

        Ok(Some(out))
    }

    fn add_unit(
        &mut self,
        unit: &CompiledUnit,
        source_path: &Path,
        files: &MappedFiles,
        package_id: MoveAddress,
    ) -> Result<(), MovyError> {
        let module = &unit.module;
        let mut module_id = module.self_id();
        module_id = ModuleId::new(package_id.into(), module_id.name().to_owned());
        let source_path = source_path
            .canonicalize()
            .unwrap_or_else(|_| source_path.to_path_buf());

        let file = self
            .files
            .entry(source_path.clone())
            .or_insert_with(|| FileCoverageMap {
                functions: BTreeMap::new(),
                lines: BTreeSet::new(),
            });

        for (idx, fdef) in module.function_defs().iter().enumerate() {
            let findex = FunctionDefinitionIndex(idx as u16);
            let function_name = module
                .identifier_at(module.function_handle_at(fdef.function).name)
                .to_string();
            self.function_to_source.insert(
                FunctionLocation {
                    module: module_id.clone(),
                    function: idx as u16,
                },
                FunctionSource {
                    file: source_path.clone(),
                    name: function_name.clone(),
                },
            );
            if let Ok(fmap) = unit.source_map.get_function_source_map(findex) {
                if let Some(line) = line_for_loc(&unit.source_map, files, &fmap.definition_location)
                {
                    file.functions.entry(function_name).or_insert(line);
                    file.lines.insert(line);
                }
            }

            let Some(code) = &fdef.code else {
                continue;
            };
            for pc in 0..code.code.len() {
                let Ok(loc) = unit.source_map.get_code_location(findex, pc as u16) else {
                    continue;
                };
                let Some(line) = line_for_loc(&unit.source_map, files, &loc) else {
                    continue;
                };
                file.lines.insert(line);
                self.pc_to_line.insert(
                    BytecodeLocation {
                        module: module_id.clone(),
                        function: idx as u16,
                        pc: pc as u16,
                    },
                    SourceLine {
                        file: source_path.clone(),
                        line,
                    },
                );
            }
        }

        Ok(())
    }

    pub fn write_lcov<I>(&self, hits: I, output: &Path) -> Result<(), MovyError>
    where
        I: IntoIterator<Item = BytecodeLocation>,
    {
        let mut line_hits = BTreeMap::<PathBuf, BTreeSet<usize>>::new();
        let mut function_hits = BTreeMap::<PathBuf, BTreeSet<String>>::new();
        for hit in hits {
            if let Some(source) = self.function_to_source.get(&FunctionLocation {
                module: hit.module.clone(),
                function: hit.function,
            }) {
                function_hits
                    .entry(source.file.clone())
                    .or_default()
                    .insert(source.name.clone());
            }

            let Some(line) = self.pc_to_line.get(&hit) else {
                continue;
            };
            line_hits
                .entry(line.file.clone())
                .or_default()
                .insert(line.line);
        }

        let mut out = String::new();
        for (path, file) in &self.files {
            let lines = line_hits.get(path);
            let hit_functions = function_hits.get(path);
            writeln!(out, "SF:{}", path.display()).unwrap();
            for (name, start_line) in &file.functions {
                writeln!(out, "FN:{start_line},{name}").unwrap();
            }
            for name in file.functions.keys() {
                let count = hit_functions
                    .map(|hit| u64::from(hit.contains(name)))
                    .unwrap_or_default();
                writeln!(out, "FNDA:{count},{name}").unwrap();
            }
            writeln!(out, "FNF:{}", file.functions.len()).unwrap();
            writeln!(
                out,
                "FNH:{}",
                hit_functions.map(|hit| hit.len()).unwrap_or_default()
            )
            .unwrap();

            for line in &file.lines {
                let count = u64::from(lines.is_some_and(|hits| hits.contains(line)));
                writeln!(out, "DA:{line},{count}").unwrap();
            }
            writeln!(out, "LF:{}", file.lines.len()).unwrap();
            writeln!(
                out,
                "LH:{}",
                lines.map(|hits| hits.len()).unwrap_or_default()
            )
            .unwrap();
            writeln!(out, "end_of_record").unwrap();
        }

        std::fs::write(output, out)
            .map_err(|e| eyre!("failed to write lcov {}: {e}", output.display()).into())
    }
}

fn line_for_loc(
    source_map: &SourceMap,
    files: &MappedFiles,
    loc: &move_ir_types::location::Loc,
) -> Option<usize> {
    if !loc.is_valid() || source_map.from_file_path.is_none() {
        return None;
    }
    files.start_position_opt(loc).map(|pos| pos.user_line())
}
