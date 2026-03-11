use harvest_benchmark::error::HarvestResult;
use harvest_core::fs::RawDir;
use harvest_core::HarvestIR;
use std::path::PathBuf;

use full_source::{CargoPackage, RawSource};
use try_cargo_build::CargoBuildResult;

/// Extract a single CargoPackage representation from the IR.
/// Returns an error if there are 0 or multiple CargoPackage representations.
pub fn raw_cargo_package(ir: &HarvestIR) -> HarvestResult<&RawDir> {
    let cargo_packages: Vec<&RawDir> = ir
        .get_by_representation::<CargoPackage>()
        .map(|(_, r)| &r.dir)
        .collect();

    match cargo_packages.len() {
        0 => Err("No CargoPackage representation found in IR".into()),
        1 => Ok(cargo_packages[0]),
        n => Err(format!(
            "Found {} CargoPackage representations, expected at most 1",
            n
        )
        .into()),
    }
}

/// Extract a single RawSource representation from the IR.
/// Returns an error if there are 0 or multiple RawSource representations.
pub fn raw_source(ir: &HarvestIR) -> HarvestResult<&RawDir> {
    let raw_sources: Vec<&RawDir> = ir
        .get_by_representation::<RawSource>()
        .map(|(_, r)| &r.dir)
        .collect();

    match raw_sources.len() {
        0 => Err("No RawSource representation found in IR".into()),
        1 => Ok(raw_sources[0]),
        n => Err(format!("Found {} RawSource representations, expected at most 1", n).into()),
    }
}

/// Extract cargo build results from the IR.
/// Returns the build artifacts or an error if no results or multiple results are found.
pub fn cargo_build_result(ir: &HarvestIR) -> Result<Vec<PathBuf>, String> {
    let build_results: Vec<Result<Vec<PathBuf>, String>> = ir
        .get_by_representation::<CargoBuildResult>()
        .map(|(_, r)| r.result.clone())
        .collect();

    match build_results.len() {
        0 => Err("No artifacts built".into()),
        1 => build_results[0].clone(),
        n => Err(format!("Found {} build results, expected at most 1", n)),
    }
}
