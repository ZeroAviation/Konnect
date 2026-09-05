use anyhow::{bail, Context, Result};
use konnect_ipc::KiCadIpcClient;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_default();
    let requested = args.next().map(PathBuf::from).with_context(|| {
        format!(
            "usage: {} <absolute-path-to-SmartMAF.kicad_pcb>",
            PathBuf::from(&program).display()
        )
    })?;
    if args.next().is_some() {
        bail!(
            "usage: {} <absolute-path-to-SmartMAF.kicad_pcb>",
            PathBuf::from(program).display()
        );
    }

    let requested = std::fs::canonicalize(&requested)
        .with_context(|| format!("cannot resolve requested board '{}'", requested.display()))?;
    let is_smartmaf = requested
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("SmartMAF.kicad_pcb"));
    if !is_smartmaf {
        bail!(
            "refusing revert: requested file is not SmartMAF.kicad_pcb: '{}'",
            requested.display()
        );
    }

    let client = KiCadIpcClient::new("");
    let open_boards = client.get_open_board_paths()?;
    if open_boards.len() != 1 {
        bail!(
            "refusing revert: expected exactly one open PCB, found {} ({})",
            open_boards.len(),
            open_boards
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    client.find_open_board(&requested).with_context(|| {
        format!(
            "refusing revert because the sole open PCB is not '{}'",
            requested.display()
        )
    })?;

    client.run_action("common.Control.revert")?;
    println!(
        "KiCad accepted the interactive revert action for '{}'.",
        requested.display()
    );
    Ok(())
}
