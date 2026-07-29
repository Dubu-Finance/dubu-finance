//! Writes the PropCurve differential test vectors that `contracts/test/` replays.
//!
//! ```text
//! cargo run --bin gen-vectors            # default path, next to the contracts
//! cargo run --bin gen-vectors -- <path>  # anywhere else
//! ```
//!
//! The default target is resolved from `CARGO_MANIFEST_DIR`, so it lands in the right place
//! regardless of the working directory. Foundry's `fs_permissions` already grants read-write
//! on `./testdata`.

use std::path::PathBuf;
use std::process::ExitCode;

use dubu_core::vectors;

/// `engine/crates/dubu-core` -> repo root -> `contracts/testdata/curve_vectors.json`.
fn default_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..3 {
        p.pop();
    }
    p.push("contracts");
    p.push("testdata");
    p.push("curve_vectors.json");
    p
}

fn main() -> ExitCode {
    let path = std::env::args_os()
        .nth(1)
        .map_or_else(default_path, PathBuf::from);

    let all = vectors::generate();
    for v in &all {
        assert!(
            vectors::verify(v),
            "vector `{}` does not reproduce its own output",
            v.name
        );
    }

    let json = match vectors::to_json(&all) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("gen-vectors: serialising failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("gen-vectors: cannot create {}: {e}", dir.display());
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = std::fs::write(&path, json.as_bytes()) {
        eprintln!("gen-vectors: cannot write {}: {e}", path.display());
        return ExitCode::FAILURE;
    }

    println!(
        "gen-vectors: wrote {} vectors ({} bytes) to {}",
        all.len(),
        json.len(),
        path.display()
    );
    ExitCode::SUCCESS
}
