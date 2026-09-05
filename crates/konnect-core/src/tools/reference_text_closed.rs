//! Closed-board Reference-field placement through KiCad's native pcbnew API.

use anyhow::{bail, Context, Result};
use konnect_ipc::types::{IpcReferenceTextBatchResult, IpcReferenceTextPlacement};
use konnect_sexp::writer::{read_consistent, write_atomic_if_unchanged};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

const WORKER: &str = include_str!("reference_text_closed.py");

/// Native pcbnew may persist project settings alongside a temporary board.
/// Track only exact reserved stems, never wildcard-match project files.
struct NativeScratchSidecars(Vec<PathBuf>);

impl NativeScratchSidecars {
    fn reserve(boards: &[&Path]) -> Result<Self> {
        let mut paths = Vec::new();
        for board in boards {
            for extension in ["kicad_pro", "kicad_prl"] {
                let sidecar = board.with_extension(extension);
                match std::fs::symlink_metadata(&sidecar) {
                    Ok(_) => bail!(
                        "native scratch sidecar already exists: '{}'",
                        sidecar.display()
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        paths.push(sidecar)
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "cannot reserve native scratch sidecar '{}'",
                                sidecar.display()
                            )
                        })
                    }
                }
            }
        }
        Ok(Self(paths))
    }
}

impl Drop for NativeScratchSidecars {
    fn drop(&mut self) {
        for path in &self.0 {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %path.display(), %error, "cannot remove native scratch sidecar");
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkerResult {
    status: String,
    kicad_version: String,
    source_sha256: String,
    candidate_sha256: String,
    requested_count: usize,
    changed_count: usize,
    unchanged_count: usize,
    placements: Vec<IpcReferenceTextPlacement>,
    normalized_non_reference_equal: bool,
}

#[derive(Debug)]
pub(crate) struct ClosedReferenceTextResult {
    pub batch: IpcReferenceTextBatchResult,
    pub source_sha256: String,
    pub result_sha256: String,
    pub kicad_version: String,
}

fn sha256(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

pub(crate) fn validate_expected_sha256(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("expected_sha256 must contain exactly 64 hexadecimal characters");
    }
    Ok(normalized)
}

fn refuse_kicad_lock(board: &Path) -> Result<()> {
    let parent = board
        .parent()
        .context("board path has no parent directory")?;
    let name = board
        .file_name()
        .context("board path has no filename")?
        .to_string_lossy();
    let project_name = board.with_extension("kicad_pro");
    let project_name = project_name
        .file_name()
        .context("project path has no filename")?
        .to_string_lossy();
    for lock in [
        parent.join(format!("~{name}.lck")),
        parent.join(format!("{name}.lck")),
        board.with_extension("lck"),
        parent.join(format!("~{project_name}.lck")),
        parent.join(format!("{project_name}.lck")),
    ] {
        match std::fs::symlink_metadata(&lock) {
            Ok(_) => bail!("KiCad lock exists at '{}'; close KiCad and reconcile the saved board before retrying", lock.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error).with_context(|| format!("cannot inspect KiCad lock '{}'", lock.display())),
        }
    }
    Ok(())
}

fn kicad_process_in_listing(listing: &str) -> Result<Option<String>> {
    if listing.trim().is_empty() {
        bail!("empty process listing cannot establish that KiCad is closed");
    }
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        let name = line.trim();
        let name = Path::new(name)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(name)
            .to_ascii_lowercase();
        if ["kicad", "pcbnew", "eeschema"].contains(&name.strip_suffix(".exe").unwrap_or(&name)) {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// IPC reachability does not prove closure: the API can be disabled, or the
/// configured socket can target another process. Fail closed on either a GUI
/// process, a board lock, or an unavailable process inventory.
async fn ensure_kicad_closed(board: &Path) -> Result<()> {
    refuse_kicad_lock(board)?;
    #[cfg(target_os = "windows")]
    let mut command = {
        // Get-Process succeeds in the desktop sandbox where tasklist may
        // return Access Denied. The fixed command emits names only and turns
        // inventory errors into a nonzero exit status.
        let mut command = tokio::process::Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$ErrorActionPreference = 'Stop'; Get-Process | ForEach-Object { $_.ProcessName }",
            ])
            .creation_flags(0x08000000);
        command
    };
    #[cfg(not(target_os = "windows"))]
    let mut command = {
        let mut command = tokio::process::Command::new("ps");
        command.args(["-A", "-o", "comm="]);
        command
    };
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .context("process inventory timed out; cannot establish that KiCad is closed")?
        .context("cannot enumerate processes to establish that KiCad is closed")?;
    if !output.status.success() {
        bail!(
            "process inventory failed; cannot establish that KiCad is closed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if let Some(name) = kicad_process_in_listing(&String::from_utf8_lossy(&output.stdout))? {
        bail!("KiCad GUI process '{name}' is running; close every KiCad editor and project manager before this closed-board operation");
    }
    // Detect a lock created during the process inventory as well.
    refuse_kicad_lock(board)
}

fn find_kicad_python(configured_cli: &str) -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("KONNECT_KICAD_PYTHON") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "KONNECT_KICAD_PYTHON points to missing file '{}'",
            path.display()
        );
    }

    let cli = crate::kicad_install::find_cli(configured_cli)
        .context("cannot locate kicad-cli to resolve KiCad's bundled Python")?;
    let bin = cli
        .parent()
        .context("resolved kicad-cli has no parent directory")?;
    #[cfg(target_os = "windows")]
    let candidates = [bin.join("python.exe")];
    #[cfg(not(target_os = "windows"))]
    let candidates = [bin.join("python3"), bin.join("python")];
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .with_context(|| {
            format!(
                "KiCad's pcbnew Python runtime was not found beside '{}'",
                cli.display()
            )
        })
}

pub(crate) async fn apply_closed_reference_texts(
    board: &Path,
    expected_sha256: &str,
    placements: &[IpcReferenceTextPlacement],
    configured_cli: &str,
) -> Result<ClosedReferenceTextResult> {
    let expected_sha256 = validate_expected_sha256(expected_sha256)?;
    let board = std::fs::canonicalize(board).context("cannot resolve source board path")?;
    let board = board.as_path();
    ensure_kicad_closed(board).await?;
    let source = read_consistent(board).context("cannot read source board consistently")?;
    let actual_sha256 = sha256(source.as_bytes());
    if actual_sha256 != expected_sha256 {
        bail!(
            "board revision mismatch: expected SHA-256 {expected_sha256}, actual {actual_sha256}"
        );
    }

    let parent = board
        .parent()
        .context("board path has no parent directory")?;
    let support = tempfile::Builder::new()
        .prefix(".konnect-reference-text-")
        .tempdir_in(parent)
        .context("cannot create Reference update support workspace")?;
    let script = support.path().join("reference_text_closed.py");
    let plan = support.path().join("placements.json");
    // Keep both native SaveBoard outputs in the board's exact directory.
    // Relative model/path serialization can depend on the destination parent,
    // so a child temp directory is not an equivalent control.
    let control = tempfile::Builder::new()
        .prefix(".konnect-reference-control-")
        .suffix(".kicad_pcb")
        .tempfile_in(parent)
        .context("cannot reserve same-directory native control board")?
        .into_temp_path();
    let candidate = tempfile::Builder::new()
        .prefix(".konnect-reference-candidate-")
        .suffix(".kicad_pcb")
        .tempfile_in(parent)
        .context("cannot reserve same-directory native candidate board")?
        .into_temp_path();
    let frozen_source = tempfile::Builder::new()
        .prefix(".konnect-reference-source-")
        .suffix(".kicad_pcb")
        .tempfile_in(parent)
        .context("cannot reserve same-directory frozen source board")?
        .into_temp_path();
    let _scratch_sidecars =
        NativeScratchSidecars::reserve(&[&control, &candidate, &frozen_source])?;
    std::fs::write(&frozen_source, source.as_bytes())
        .context("cannot freeze source board bytes")?;
    std::fs::write(&script, WORKER).context("cannot write temporary pcbnew worker")?;
    std::fs::write(
        &plan,
        serde_json::to_vec(&serde_json::json!({ "placements": placements }))?,
    )
    .context("cannot write temporary Reference placement plan")?;

    let python = find_kicad_python(configured_cli)?;
    let mut command = tokio::process::Command::new(&python);
    #[cfg(target_os = "windows")]
    command.creation_flags(0x08000000);
    command
        .kill_on_drop(true)
        .arg(&script)
        .arg(&frozen_source)
        .arg(&control)
        .arg(&candidate)
        .arg(&plan)
        .arg(&expected_sha256);
    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .context("native pcbnew Reference update timed out after 120 seconds")?
        .with_context(|| format!("cannot launch KiCad Python '{}'", python.display()))?;
    if !output.status.success() {
        bail!(
            "native pcbnew Reference update failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let worker: WorkerResult = serde_json::from_slice(&output.stdout)
        .context("native pcbnew worker returned malformed verification JSON")?;
    if worker.status != "PASS"
        || !worker.normalized_non_reference_equal
        || worker.source_sha256 != expected_sha256
        || worker.requested_count != placements.len()
        || worker.changed_count + worker.unchanged_count != placements.len()
        || worker.placements.len() != placements.len()
    {
        bail!("native pcbnew worker returned inconsistent verification results");
    }

    // This private worker output has no participating concurrent writers. A
    // document-lock reader here would leave a permanent lock sidecar for every
    // temporary candidate; the verified content hash supplies integrity.
    let candidate_content =
        std::fs::read_to_string(&candidate).context("cannot read native pcbnew candidate")?;
    let candidate_sha256 = sha256(candidate_content.as_bytes());
    if candidate_sha256 != worker.candidate_sha256 {
        bail!("native pcbnew candidate SHA-256 does not match its verified report");
    }

    ensure_kicad_closed(board).await?;
    // Even an idempotent request must not report success against a stale source.
    if read_consistent(board).context("cannot recheck source board")? != source {
        bail!("board changed during native Reference verification; candidate was not applied");
    }
    if worker.changed_count > 0 {
        write_atomic_if_unchanged(board, &source, &candidate_content)
            .context("board changed before atomic Reference update")?;
        let written = read_consistent(board).context("cannot read back updated board")?;
        if sha256(written.as_bytes()) != candidate_sha256 {
            bail!("atomic Reference update readback SHA-256 mismatch");
        }
    }

    Ok(ClosedReferenceTextResult {
        batch: IpcReferenceTextBatchResult {
            requested_count: worker.requested_count,
            changed_count: worker.changed_count,
            unchanged_count: worker.unchanged_count,
            placements: worker.placements,
        },
        source_sha256: expected_sha256,
        result_sha256: if worker.changed_count == 0 {
            actual_sha256
        } else {
            candidate_sha256
        },
        kicad_version: worker.kicad_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_text_scratch_cleanup_removes_only_reserved_native_sidecars() {
        let directory = tempfile::tempdir().unwrap();
        let board = directory.path().join(".konnect-reference-test.kicad_pcb");
        let production_project = directory.path().join("SmartMAF.kicad_pro");
        let unrelated = directory.path().join(".konnect-reference-other.kicad_pro");
        std::fs::write(&production_project, "real settings").unwrap();
        std::fs::write(&unrelated, "unrelated settings").unwrap();
        {
            let _cleanup = NativeScratchSidecars::reserve(&[&board]).unwrap();
            std::fs::write(board.with_extension("kicad_pro"), "native project").unwrap();
            std::fs::write(board.with_extension("kicad_prl"), "native local settings").unwrap();
        }
        assert!(!board.with_extension("kicad_pro").exists());
        assert!(!board.with_extension("kicad_prl").exists());
        assert_eq!(
            std::fs::read_to_string(&production_project).unwrap(),
            "real settings"
        );
        assert_eq!(
            std::fs::read_to_string(&unrelated).unwrap(),
            "unrelated settings"
        );
        std::fs::write(board.with_extension("kicad_pro"), "pre-existing settings").unwrap();
        assert!(NativeScratchSidecars::reserve(&[&board]).is_err());
        assert_eq!(
            std::fs::read_to_string(board.with_extension("kicad_pro")).unwrap(),
            "pre-existing settings"
        );
    }

    #[test]
    fn reference_text_lock_guard_checks_every_supported_lock_name() {
        let directory = tempfile::tempdir().unwrap();
        let board = directory.path().join("board.kicad_pcb");
        for name in [
            "~board.kicad_pcb.lck",
            "board.kicad_pcb.lck",
            "board.lck",
            "~board.kicad_pro.lck",
            "board.kicad_pro.lck",
        ] {
            let lock = directory.path().join(name);
            std::fs::write(&lock, "lock").unwrap();
            assert!(refuse_kicad_lock(&board).is_err(), "{name}");
            std::fs::remove_file(lock).unwrap();
        }
        assert!(refuse_kicad_lock(&board).is_ok());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reference_text_process_guard_requires_valid_inventory_and_no_gui() {
        assert!(kicad_process_in_listing("").is_err());
        assert_eq!(
            kicad_process_in_listing("python\r\nkonnect\r\npowershell").unwrap(),
            None
        );
        for name in ["pcbnew.exe", "KiCad", "eeschema"] {
            let line = format!("python\r\n{name}\r\npowershell");
            assert!(kicad_process_in_listing(&line).unwrap().is_some(), "{name}");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn reference_text_python_syntax_and_immutable_regressions() {
        let python = find_kicad_python(r"C:\Program Files\KiCad\10.0\bin\kicad-cli.exe").unwrap();
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tools");
        let syntax = std::process::Command::new(&python).args([
            "-c", "import ast,pathlib; [ast.parse(p.read_text()) for p in pathlib.Path('.').glob('reference_text_closed*.py')]",
        ]).current_dir(&path).output().unwrap();
        assert!(
            syntax.status.success(),
            "{}",
            String::from_utf8_lossy(&syntax.stderr)
        );
        let tests = std::process::Command::new(&python)
            .arg(path.join("reference_text_closed_test.py"))
            .output()
            .unwrap();
        assert!(
            tests.status.success(),
            "{}",
            String::from_utf8_lossy(&tests.stderr)
        );
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn native_worker_refuses_kicad_lock_without_replacing_private_board() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../konnect-sexp/tests/fixtures/placement/placement_fixture.kicad_pcb");
        let directory = tempfile::tempdir().unwrap();
        let board = directory.path().join("locked.kicad_pcb");
        std::fs::copy(fixture, &board).unwrap();
        let original = std::fs::read(&board).unwrap();
        std::fs::write(
            directory.path().join("~locked.kicad_pcb.lck"),
            "held by editor",
        )
        .unwrap();
        let result = apply_closed_reference_texts(
            &board,
            &sha256(&original),
            &[IpcReferenceTextPlacement {
                reference: "R1".into(),
                x: 14.25,
                y: 24.5,
                rotation: 90.0,
                size_x: 0.8,
                size_y: 0.8,
                stroke_width: 0.15,
            }],
            r"C:\Program Files\KiCad\10.0\bin\kicad-cli.exe",
        )
        .await;
        assert!(result.is_err(), "a KiCad lock must prevent replacement");
        assert_eq!(std::fs::read(&board).unwrap(), original);
        assert!(format!("{:#}", result.unwrap_err()).contains("KiCad lock"));
    }

    #[test]
    fn expected_sha256_is_exact_and_case_normalized() {
        assert_eq!(
            validate_expected_sha256(&"A".repeat(64)).unwrap(),
            "a".repeat(64)
        );
        for bad in ["", "abc", &"g".repeat(64), &"a".repeat(63)] {
            assert!(validate_expected_sha256(bad).is_err());
        }
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn native_worker_changes_only_a_reference_on_a_private_real_board_copy() {
        let cli = PathBuf::from(r"C:\Program Files\KiCad\10.0\bin\kicad-cli.exe");
        if !cli.is_file() {
            return;
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../konnect-sexp/tests/fixtures/placement/placement_fixture.kicad_pcb");
        let directory = tempfile::tempdir().unwrap();
        let board = directory.path().join("placement_fixture.kicad_pcb");
        std::fs::copy(&fixture, &board).unwrap();
        let original_fixture = std::fs::read(&fixture).unwrap();
        let source = std::fs::read(&board).unwrap();
        let expected = sha256(&source);
        let placement = IpcReferenceTextPlacement {
            reference: "R1".into(),
            x: 14.25,
            y: 24.5,
            rotation: 90.0,
            size_x: 0.8,
            size_y: 0.8,
            stroke_width: 0.15,
        };

        let result = apply_closed_reference_texts(
            &board,
            &expected,
            std::slice::from_ref(&placement),
            cli.to_str().unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(result.batch.requested_count, 1);
        assert_eq!(result.batch.changed_count, 1);
        assert_eq!(result.batch.placements, vec![placement]);
        assert_ne!(result.source_sha256, result.result_sha256);
        assert_eq!(std::fs::read(&fixture).unwrap(), original_fixture);
        assert_eq!(
            sha256(&std::fs::read(&board).unwrap()),
            result.result_sha256
        );
        let after_first = std::fs::read(&board).unwrap();
        let second = apply_closed_reference_texts(
            &board,
            &result.result_sha256,
            &result.batch.placements,
            cli.to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(second.batch.changed_count, 0);
        assert_eq!(second.batch.unchanged_count, 1);
        assert_eq!(second.result_sha256, result.result_sha256);
        assert_eq!(std::fs::read(&board).unwrap(), after_first);
        let native_sidecars: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".konnect-reference-")
                    && matches!(
                        path.extension().and_then(|value| value.to_str()),
                        Some("kicad_pro" | "kicad_prl")
                    )
            })
            .collect();
        assert!(
            native_sidecars.is_empty(),
            "native scratch sidecars leaked: {native_sidecars:?}"
        );
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn native_worker_verifies_all_69_smartmaf_references_on_a_private_copy() {
        let cli = PathBuf::from(r"C:\Program Files\KiCad\10.0\bin\kicad-cli.exe");
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
        let source = repository.join("docs/hardware/smartmaf/fixes_2026-09-04/reference_text_prechange_reserialized.kicad_pcb");
        let plan_path = repository
            .join("docs/hardware/smartmaf/fixes_2026-09-04/designator_audit_revised.json");
        if !cli.is_file() || !source.is_file() || !plan_path.is_file() {
            return;
        }
        let source_bytes = std::fs::read(&source).unwrap();
        let expected = sha256(&source_bytes);
        assert_eq!(
            expected,
            "eb89f5a33159ef43c80c02f27f7c000c45cf8477ab77b22b1b50c989c35da767"
        );
        let plan: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&plan_path).unwrap()).unwrap();
        assert_eq!(plan["source_sha256"], expected);
        let placements: Vec<IpcReferenceTextPlacement> = plan["proposal"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| IpcReferenceTextPlacement {
                reference: row["reference"].as_str().unwrap().to_string(),
                x: row["x"].as_f64().unwrap(),
                y: row["y"].as_f64().unwrap(),
                rotation: row["rotation"].as_f64().unwrap(),
                size_x: row["text_width_mm"].as_f64().unwrap(),
                size_y: row["text_height_mm"].as_f64().unwrap(),
                stroke_width: row["thickness_mm"].as_f64().unwrap(),
            })
            .collect();
        assert_eq!(placements.len(), 69);

        let directory = tempfile::tempdir().unwrap();
        let copy = directory.path().join("SmartMAF.kicad_pcb");
        std::fs::copy(&source, &copy).unwrap();
        let result =
            apply_closed_reference_texts(&copy, &expected, &placements, cli.to_str().unwrap())
                .await
                .unwrap();

        assert_eq!(result.batch.requested_count, 69);
        assert_eq!(result.batch.placements, placements);
        assert_eq!(result.batch.changed_count, 69);
        assert_eq!(
            result.result_sha256,
            "7c5efe25a18dc4d544fd9fd541dffaee44bd3fd46ee6d1195b2626dcf3ddfb48"
        );
        assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
        assert_eq!(sha256(&std::fs::read(&copy).unwrap()), result.result_sha256);
        let after_first = std::fs::read(&copy).unwrap();
        let second = apply_closed_reference_texts(
            &copy,
            &result.result_sha256,
            &placements,
            cli.to_str().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(second.batch.changed_count, 0);
        assert_eq!(second.batch.unchanged_count, 69);
        assert_eq!(second.result_sha256, result.result_sha256);
        assert_eq!(std::fs::read(&copy).unwrap(), after_first);
    }
}
