//! Opt-in retention of complete Cargo build units for named, persistent sessions.
//!
//! The state lock precedes layout locks. Collection takes other profiles' locks
//! non-blockingly, and graph leases non-blockingly, so it never waits for a
//! running program. Only directories recorded by this feature are collected.

use super::{BuildContext, BuildRunner, Unit};
use crate::ops::CompileOptions;
use crate::util::flock::{FileLock, Filesystem, is_on_nfs_mount};
use crate::util::{CargoResult, GlobalContext, short_hash};
use crate::workspace::Workspace;
use anyhow::{Context as _, bail};
use cargo_util::paths;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

const VERSION: u32 = 2;
const STATE_DIR: &str = ".cargo-active-artifacts";

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    sessions: BTreeMap<String, Session>,
    /// Only known units are eligible. Existing caches are never adopted by scan.
    units: BTreeMap<PathBuf, Option<String>>,
    /// Published output paths pin their complete transitive backing graph.
    outputs: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    /// One kernel lease per complete executing graph, including orphaned jobs.
    executions: BTreeMap<String, BTreeSet<PathBuf>>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    configuration: String,
    slots: BTreeMap<String, BTreeSet<PathBuf>>,
}

pub(crate) struct ActiveArtifacts {
    root: PathBuf,
    raw_root: PathBuf,
    state_dir: Filesystem,
    _lock: FileLock,
    state: State,
    session: String,
    configuration: String,
    slot: String,
    current: BTreeSet<PathBuf>,
    outputs: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

impl ActiveArtifacts {
    pub(crate) fn begin(
        bcx: &BuildContext<'_, '_>,
        options: &CompileOptions,
    ) -> CargoResult<Option<Self>> {
        let gctx = bcx.gctx;
        let cfg = gctx.build_config()?;
        if !gctx.cli_unstable().active_artifacts {
            if cfg.artifact_session.is_some()
                || cfg.artifact_configuration.is_some()
                || cfg.artifact_slot.is_some()
            {
                bail!("artifact retention configuration requires -Zactive-artifacts");
            }
            return Ok(None);
        }
        if !gctx.cli_unstable().build_dir_new_layout {
            bail!("-Zactive-artifacts requires the new build-dir layout");
        }
        if gctx.cli_unstable().fine_grain_locking {
            bail!("-Zactive-artifacts currently requires coarse build-directory locking");
        }
        let session = cfg
            .artifact_session
            .clone()
            .unwrap_or_else(|| format!("workspace-{}", short_hash(&bcx.ws.root())));
        if session.is_empty() {
            bail!("build.artifact-session must not be empty");
        }

        // Mode and target selection belong to workflow slots, not configuration
        // identity. In particular dev/test and release/bench share directory names.
        let flags: BTreeSet<_> = bcx
            .build_config
            .requested_kinds
            .iter()
            .chain(std::iter::once(&super::CompileKind::Host))
            .map(|kind| {
                let info = bcx.target_data.info(*kind);
                (
                    format!("{kind:?}"),
                    info.rustflags.to_vec(),
                    info.rustdocflags.to_vec(),
                )
            })
            .collect();
        let configuration = short_hash(&(
            VERSION,
            &bcx.rustc().verbose_version,
            &options.cli_features,
            bcx.profiles.get_dir_name(),
            bcx.profiles.artifact_retention_settings()?,
            flags,
            &options.target_rustc_args,
            &options.target_rustdoc_args,
            &options.target_rustc_crate_types,
            &cfg.artifact_configuration,
        ));
        // Replacing the last graph per operation collects removed dependencies.
        // An explicit slot supports two check target selections kept warm together.
        let slot = cfg.artifact_slot.clone().unwrap_or_else(|| {
            short_hash(&format!(
                "{:?}:{:?}:{:?}",
                bcx.build_config.intent,
                bcx.build_config.primary_unit_rustc,
                (
                    gctx.get_env_os("RUSTC_WRAPPER"),
                    gctx.get_env_os("RUSTC_WORKSPACE_WRAPPER"),
                    &cfg.rustc_wrapper,
                    &cfg.rustc_workspace_wrapper
                )
            ))
        });
        Self::open(bcx.ws, session, configuration, slot).map(Some)
    }

    fn open(
        ws: &Workspace<'_>,
        session: String,
        configuration: String,
        slot: String,
    ) -> CargoResult<Self> {
        let raw_root = ws.build_dir().into_path_unlocked();
        paths::create_dir_all_excluded_from_backups_atomic(&raw_root)?;
        let root = fs::canonicalize(&raw_root)?;
        if is_on_nfs_mount(&root) {
            bail!("active artifact retention requires reliable local filesystem locks");
        }
        let state_dir = Filesystem::new(root.join(STATE_DIR));
        paths::create_dir_all(state_dir.as_path_unlocked())?;
        let lock =
            state_dir.open_rw_exclusive_create("lock", ws.gctx(), "active artifact sessions")?;
        let state_path = state_dir.as_path_unlocked().join("state.json");
        let state = match fs::read(&state_path) {
            Ok(bytes) => {
                let state: State = serde_json::from_slice(&bytes)
                    .context("invalid active artifact state; refusing to collect")?;
                if state.version != VERSION {
                    bail!("unsupported active artifact state version; refusing to collect");
                }
                state
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State {
                version: VERSION,
                ..State::default()
            },
            Err(e) => return Err(e).context("failed to read active artifact state"),
        };
        for path in state
            .units
            .keys()
            .chain(
                state
                    .sessions
                    .values()
                    .flat_map(|s| s.slots.values().flatten()),
            )
            .chain(state.outputs.values().flatten())
            .chain(state.executions.values().flatten())
        {
            validate_unit(path)?;
        }
        for (key, graph) in &state.executions {
            if key != &short_hash(graph) {
                bail!("invalid artifact graph lease; refusing to collect");
            }
        }
        Ok(Self {
            root,
            raw_root,
            state_dir,
            _lock: lock,
            state,
            session,
            configuration,
            slot,
            current: BTreeSet::new(),
            outputs: BTreeMap::new(),
        })
    }

    /// Called after layout locks are held, before any compiler job starts.
    pub(crate) fn prepare(&mut self, runner: &BuildRunner<'_, '_>) -> CargoResult<Vec<FileLock>> {
        for unit in runner.bcx.unit_graph.keys() {
            let dir = self.unit_dir(runner, unit)?;
            if self.current.insert(dir.clone()) {
                self.state.units.entry(dir.clone()).or_insert(None);
            }
        }
        let key = short_hash(&self.current);
        let lease = self.state_dir.open_ro_shared_create(
            format!("leases/{key}"),
            runner.bcx.gctx,
            "artifact graph execution lease",
        )?;
        self.state.executions.insert(key, self.current.clone());
        for unit in &runner.bcx.roots {
            if unit.mode.is_doc_test() {
                continue;
            }
            let mut backing = BTreeSet::new();
            self.backing(runner, unit, &mut backing)?;
            for output in runner.outputs(unit)?.iter() {
                for path in output.hardlink.iter().chain(output.export_path.iter()) {
                    self.outputs.insert(path.clone(), backing.clone());
                }
            }
        }
        // Compilation can publish an output before another job or the warning
        // policy fails. Until successful commit identifies its exact backing,
        // keep both the previous and projected graphs for that output alive.
        for (path, graph) in &self.outputs {
            self.state
                .outputs
                .entry(path.clone())
                .or_default()
                .extend(graph.iter().cloned());
        }
        // A crash or compile failure leaves the old session roots intact and a
        // durable ledger of new candidates. Nothing is evicted before success.
        self.save()?;
        Ok(vec![lease])
    }

    fn unit_dir(&self, runner: &BuildRunner<'_, '_>, unit: &Unit) -> CargoResult<PathBuf> {
        let lock = runner.files().build_unit_lock(unit);
        let dir = lock
            .parent()
            .unwrap()
            .strip_prefix(&self.raw_root)
            .context("build unit is outside the retention directory")?
            .to_path_buf();
        validate_unit(&dir)?;
        Ok(dir)
    }

    fn backing(
        &self,
        runner: &BuildRunner<'_, '_>,
        unit: &Unit,
        out: &mut BTreeSet<PathBuf>,
    ) -> CargoResult<()> {
        if !out.insert(self.unit_dir(runner, unit)?) {
            return Ok(());
        }
        for dep in runner.unit_deps(unit) {
            self.backing(runner, &dep.unit, out)?;
        }
        Ok(())
    }

    pub(crate) fn commit(mut self, gctx: &GlobalContext) -> CargoResult<()> {
        let entry = self.state.sessions.entry(self.session.clone()).or_default();
        if entry.configuration != self.configuration {
            entry.slots.clear();
            entry.configuration = self.configuration.clone();
        }
        entry.slots.insert(self.slot.clone(), self.current.clone());
        for dir in &self.current {
            self.state
                .units
                .insert(dir.clone(), fingerprint_stamp(&self.root.join(dir))?);
        }
        self.state.outputs.append(&mut self.outputs);
        self.state.outputs.retain(|path, _| path.exists());
        // Publish the successful new roots atomically before touching old units.
        self.save()?;
        self.collect(gctx, false)?;
        Ok(())
    }

    fn save(&self) -> CargoResult<()> {
        paths::write_atomic(
            self.state_dir.as_path_unlocked().join("state.json"),
            serde_json::to_vec(&self.state)?,
        )
    }

    fn collect(&mut self, gctx: &GlobalContext, dry_run: bool) -> CargoResult<()> {
        let mut live: BTreeSet<_> = self
            .state
            .sessions
            .values()
            .flat_map(|s| s.slots.values().flatten())
            .cloned()
            .collect();
        live.extend(self.state.outputs.values().flatten().cloned());
        // A single descriptor protects a whole graph, so large workspaces do
        // not exhaust file limits. Children inherit it before compiler jobs.
        for (key, graph) in self.state.executions.clone() {
            if let Some(lease) = self
                .state_dir
                .try_open_rw_exclusive_create(format!("leases/{key}"))?
            {
                if !dry_run {
                    self.state.executions.remove(&key);
                    drop(lease);
                    paths::remove_file(
                        self.state_dir
                            .as_path_unlocked()
                            .join(format!("leases/{key}")),
                    )?;
                }
            } else {
                live.extend(graph);
            }
        }
        let candidates: Vec<_> = self
            .state
            .units
            .keys()
            .filter(|p| !live.contains(*p))
            .cloned()
            .collect();
        let mut removed = 0;
        // Compilation releases its layout locks before commit. Taking all three
        // locks also excludes older Cargo and independent check/build processes.
        let profiles: BTreeSet<_> = candidates
            .iter()
            .map(|p| {
                p.parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .to_path_buf()
            })
            .collect();
        for profile in profiles {
            let profile_path = self.root.join(&profile);
            if !profile_path.exists() {
                continue;
            }
            ensure_no_symlinks(&self.root, &profile)?;
            let fs = Filesystem::new(profile_path);
            let mut locks = Vec::new();
            for name in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
                if let Some(lock) = fs.try_open_rw_exclusive_create(name)? {
                    locks.push(lock);
                } else {
                    break;
                }
            }
            if locks.len() != 3 {
                continue;
            }
            for dir in candidates
                .iter()
                .filter(|p| p.parent().unwrap().parent().unwrap().parent().unwrap() == profile)
            {
                ensure_no_symlinks(&self.root, dir)?;
                let path = self.root.join(dir);
                if !path.exists() {
                    if !dry_run {
                        self.state.units.remove(dir);
                    }
                    continue;
                }
                // If an unregistered Cargo rewrote the unit, conservatively leave it.
                if let Some(Some(stamp)) = self.state.units.get(dir)
                    && fingerprint_stamp(&path)?.as_ref() != Some(stamp)
                {
                    continue;
                }
                if !dry_run {
                    paths::remove_dir_all(&path)?;
                    self.state.units.remove(dir);
                }
                removed += 1;
            }
        }
        if !dry_run {
            self.save()?;
        }
        if removed > 0 {
            gctx.shell().status(
                if dry_run { "Would remove" } else { "Collected" },
                format!("{removed} inactive build units"),
            )?;
        }
        Ok(())
    }
}

/// Release cache roots while retaining published outputs and their debug data.
pub(crate) fn release(ws: &Workspace<'_>, session: &str, dry_run: bool) -> CargoResult<()> {
    if !ws.gctx().cli_unstable().active_artifacts {
        bail!("--artifact-session requires -Zactive-artifacts");
    }
    let mut retention =
        ActiveArtifacts::open(ws, session.to_string(), String::new(), String::new())?;
    retention.state.sessions.remove(session);
    retention.state.outputs.retain(|path, _| path.exists());
    if !dry_run {
        retention.save()?;
    }
    retention.collect(ws.gctx(), dry_run)
}

fn validate_unit(path: &Path) -> CargoResult<()> {
    let parts: Vec<_> = path.components().collect();
    if !(parts.len() == 4 || parts.len() == 5)
        || parts.iter().any(|p| !matches!(p, Component::Normal(_)))
        || parts[parts.len() - 3].as_os_str() != "build"
        || !parts
            .last()
            .unwrap()
            .as_os_str()
            .to_str()
            .is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!(
            "invalid retained unit path {}; refusing to collect",
            path.display()
        );
    }
    Ok(())
}

fn ensure_no_symlinks(root: &Path, relative: &Path) -> CargoResult<()> {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_symlink() => bail!(
                "symlink in retained unit path {}; refusing to collect",
                path.display()
            ),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn fingerprint_stamp(unit: &Path) -> CargoResult<Option<String>> {
    let entries = match fs::read_dir(unit.join("fingerprint")) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut stamps = BTreeSet::new();
    for entry in entries {
        let entry = entry?;
        let meta = entry.metadata()?;
        stamps.insert((entry.file_name(), meta.len(), meta.modified()?));
    }
    Ok(Some(short_hash(&stamps)))
}
