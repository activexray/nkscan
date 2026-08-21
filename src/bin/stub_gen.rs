//! Writes `nkscan.pyi` from the bindings themselves
//!
//! An abi3 extension carries no introspectable signatures, so a consumer needs a stub to see the
//! API at all. Generating it means it cannot drift from `src/python.rs`, which a hand-written one
//! silently does.
//!
//! `cargo run --features python --bin stub_gen`

use std::fs;
use std::path::Path;

/// Where the generator puts the stub, which is what `pyproject.toml` ships
const STUB: &str = "nkscan.pyi";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    nkscan::python_stub_info()?.generate()?;
    unqualify_local_classes(Path::new(STUB))?;
    println!("wrote {STUB}");
    Ok(())
}

/// Drop the `builtins.` a locally declared class is wrongly given as a base
///
/// `pyo3_stub_gen::create_exception!` registers an exception's base as a builtin whatever it
/// actually is, so one deriving from another of ours comes out as
/// `class DeviceBusy(builtins.TransientError)`. There is no such name in `builtins` and a type
/// checker rejects it. Every class the file declares is fair game to unqualify, so this needs no
/// list to keep in step.
fn unqualify_local_classes(path: &Path) -> std::io::Result<()> {
    let stub = fs::read_to_string(path)?;
    let declared: Vec<String> = stub
        .lines()
        .filter_map(|line| line.strip_prefix("class "))
        .filter_map(|rest| rest.split(['(', ':']).next())
        .map(str::to_owned)
        .collect();

    let fixed = declared.iter().fold(stub.clone(), |stub, name| {
        stub.replace(&format!("builtins.{name}"), name)
    });
    if fixed != stub {
        fs::write(path, fixed)?;
    }
    Ok(())
}
