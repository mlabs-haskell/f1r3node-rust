use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::Command;

use models::rhoapi::Par;
use rholang_parser::SourcePos;

use crate::rust::interpreter::compiler::exports::{
    BoundMapChain, FreeMap, IdContextPos, NameVisitInputs, ProcVisitInputs,
};
use crate::rust::interpreter::compiler::normalize::VarSort;
use crate::rust::interpreter::compiler::normalize::VarSort::{NameSort, ProcSort};

// Helper for skipping PeTTa tests if runtime pre-requisites are not met (or
// panicking if tests are mandatory).
pub fn should_skip_petta_test() -> bool {
    let require = env::var_os("REQUIRE_PETTA_TESTS").is_some();

    let petta_script_path =
        PathBuf::from(env::var("PETTA_SCRIPT_PATH").unwrap_or("./petta.sh".into()));
    let petta_dir = env::var("PETTA_DIR").ok();
    let cache_dir = env::var("CACHE_DIR").ok();
    let sandbox_lib_path = env::var("SANDBOX_LIB_PATH").ok();

    let petta_script_missing = !petta_script_path.exists();
    let petta_dir_missing = petta_dir.is_none();
    let cache_dir_missing = cache_dir.is_none();
    let sandbox_lib_path_missing = sandbox_lib_path.is_none();

    let swipl_missing = Command::new("swipl")
        .arg("--version")
        .output()
        .map(|output| !output.status.success())
        .unwrap_or(true);

    let error_message = if petta_script_missing {
        "PeTTa test prerequisite unmet: petta.sh script is missing".to_string()
    } else if petta_dir_missing {
        "PeTTa test prerequisite unmet: PETTA_DIR environment variable not set".to_string()
    } else if cache_dir_missing {
        "PeTTa test prerequisite unmet: CACHE_DIR environment variable not set".to_string()
    } else if sandbox_lib_path_missing {
        "PeTTa test prerequisite unmet: SANDBOX_LIB_PATH environment variable not set".to_string()
    } else if swipl_missing {
        "PeTTa test prerequisite unmet: swipl is missing".to_string()
    } else {
        return false;
    };

    if require {
        panic!("{error_message}");
    } else {
        eprintln!("Skipping test: {error_message}");
        true
    }
}

pub fn name_visit_inputs_and_env() -> (NameVisitInputs, HashMap<String, Par>) {
    let input: NameVisitInputs = NameVisitInputs {
        bound_map_chain: BoundMapChain::default(),
        free_map: FreeMap::default(),
    };
    let env: HashMap<String, Par> = HashMap::new();

    (input, env)
}

pub fn proc_visit_inputs_and_env() -> (ProcVisitInputs, HashMap<String, Par>) {
    let proc_inputs = ProcVisitInputs {
        par: Default::default(),
        bound_map_chain: BoundMapChain::new(),
        free_map: Default::default(),
    };
    let env: HashMap<String, Par> = HashMap::new();

    (proc_inputs, env)
}

pub fn collection_proc_visit_inputs_and_env() -> (ProcVisitInputs, HashMap<String, Par>) {
    let proc_inputs = ProcVisitInputs {
        par: Default::default(),
        bound_map_chain: {
            let bound_map_chain = BoundMapChain::new();
            bound_map_chain.put_all_pos(vec![
                (
                    "P".to_string(),
                    ProcSort,
                    SourcePos { line: 1, col: 1 }, // Use 1-based indexing consistent with rholang-rs
                ),
                ("x".to_string(), NameSort, SourcePos { line: 1, col: 1 }),
            ])
        },
        free_map: Default::default(),
    };
    let env: HashMap<String, Par> = HashMap::new();

    (proc_inputs, env)
}

pub fn proc_visit_inputs_with_updated_bound_map_chain(
    input: ProcVisitInputs,
    name: &str,
    vs_type: VarSort,
) -> ProcVisitInputs {
    ProcVisitInputs {
        bound_map_chain: {
            input.bound_map_chain.put_pos((
                name.to_string(),
                vs_type,
                SourcePos { line: 1, col: 1 }, // Use 1-based indexing
            ))
        },
        ..input.clone()
    }
}

pub fn proc_visit_inputs_with_updated_vec_bound_map_chain(
    input: ProcVisitInputs,
    new_bindings: Vec<(String, VarSort)>,
) -> ProcVisitInputs {
    let bindings_with_default_positions: Vec<IdContextPos<VarSort>> = new_bindings
        .into_iter()
        .map(|(name, var_sort)| (name, var_sort, SourcePos { line: 1, col: 1 }))
        .collect();

    ProcVisitInputs {
        bound_map_chain: {
            input
                .bound_map_chain
                .put_all_pos(bindings_with_default_positions)
        },
        ..input.clone()
    }
}
