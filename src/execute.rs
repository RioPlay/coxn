use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::app::{
    AGENT_PREAMBLE_ADEN, AGENT_PREAMBLE_BASE, ModelSel, openai_model, resolve_instance_from_config,
    resolve_role, task_config,
};
use crate::model::{AnyModel, Usage};
use crate::pump::{BatchIo, Pump};
use crate::tools::register_aden_tools;
use crate::tools::{EditTool, GlobTool, GrepTool, ReadFileTool, RunTool, ToolRegistry, WriteTool};
use crate::{aden, agents, gate, provider, run_ledger};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolPolicy {
    ReadOnly,
    Edit,
    Command,
    Full,
}

impl ToolPolicy {
    pub(crate) fn for_role(role: &str) -> Self {
        match role {
            "scout" => Self::ReadOnly,
            "synth" => Self::Edit,
            "orchestrate" => Self::Command,
            _ => Self::Full,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Edit => "read+edit",
            Self::Command => "read+command",
            Self::Full => "full",
        }
    }
}

fn registry_for_policy(
    dir: &Path,
    caps: &aden::AdenCaps,
    bwrap: bool,
    policy: ToolPolicy,
) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    register_aden_tools(&mut tools, dir, caps.available);
    let root = dir.to_path_buf();
    tools.register(Box::new(ReadFileTool::new(root.clone())));
    tools.register(Box::new(GrepTool::new(root.clone())));
    tools.register(Box::new(GlobTool::new(root.clone())));
    match policy {
        ToolPolicy::ReadOnly => {}
        ToolPolicy::Edit => {
            tools.register(Box::new(EditTool::new(root.clone())));
            tools.register(Box::new(WriteTool::new(root)));
        }
        ToolPolicy::Command => {
            tools.register(Box::new(RunTool::new(root, bwrap)));
        }
        ToolPolicy::Full => {
            tools.register(Box::new(EditTool::new(root.clone())));
            tools.register(Box::new(WriteTool::new(root.clone())));
            tools.register(Box::new(RunTool::new(root, bwrap)));
        }
    }
    tools
}

/// Optional sink for live `/execute` progress (full report snapshot per update).
pub(crate) struct ExecuteProgress<'a> {
    on_update: Option<&'a mut dyn FnMut(&str)>,
    should_cancel: Option<&'a mut dyn FnMut() -> bool>,
}

impl<'a> ExecuteProgress<'a> {
    pub(crate) fn new(
        on_update: &'a mut dyn FnMut(&str),
        should_cancel: Option<&'a mut dyn FnMut() -> bool>,
    ) -> Self {
        Self {
            on_update: Some(on_update),
            should_cancel,
        }
    }

    fn emit(&mut self, report: &str) {
        if let Some(cb) = &mut self.on_update {
            cb(report);
        }
    }

    fn cancelled(&mut self) -> bool {
        self.should_cancel.as_mut().is_some_and(|f| f())
    }
}

fn append_report(report: &mut String, progress: &mut ExecuteProgress<'_>, chunk: &str) {
    report.push_str(chunk);
    progress.emit(report);
}

fn abort_report(report: &mut String, progress: &mut ExecuteProgress<'_>, msg: &str) -> String {
    append_report(report, progress, &format!("\n✗ {msg}\n"));
    std::mem::take(report)
}

/// Run the aden task partition. With `COXN_EXECUTE_JOBS > 1` (and a fresh,
/// non-resumed run) the parallel wave scheduler runs independent read-only
/// scopes concurrently on worker threads; mutating scopes always serialize on
/// the driving thread so the aden whole-tree gate never sees another agent's
/// diff. `--resume` and `jobs <= 1` take the proven sequential path.
pub(crate) async fn execute_partition(
    dir: &Path,
    caps: &aden::AdenCaps,
    sel: &ModelSel,
    bwrap: bool,
    resume: bool,
    progress: ExecuteProgress<'_>,
) -> String {
    let jobs = execute_jobs();
    if jobs > 1 && !resume {
        return execute_partition_parallel(dir, caps, sel, bwrap, jobs, progress).await;
    }
    execute_partition_sequential(dir, caps, sel, bwrap, resume, progress).await
}

/// Parse `COXN_EXECUTE_JOBS` (default `1`, clamped `1..=8`). Values above `1`
/// opt into the parallel wave scheduler for independent read-only scopes.
fn execute_jobs() -> usize {
    std::env::var("COXN_EXECUTE_JOBS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|c| *c >= 1)
        .unwrap_or(1)
        .min(8)
}

/// Refuse `/execute` when the active model or a role route is text-only CLI piggyback.
fn execute_route_guard(
    dir: &Path,
    caps: &aden::AdenCaps,
    sel: &ModelSel,
    ordered: &[&agents::SubScope],
) -> Option<String> {
    if crate::discover::is_text_only_piggyback(sel) {
        return Some(
            "active model is text-only (CLI piggyback) — /execute needs tools.\n\
             fix: route scout/synth to openai_compat or ollama in .aden/config.toml, \
             or Ctrl-Space → setup ollama-native / openrouter-claude"
                .to_string(),
        );
    }
    let cfg = provider::load_config(dir);
    for scope in ordered {
        if let Some(selection) = resolve_role(dir, caps, &scope.role)
            && crate::discover::selection_is_text_only(&cfg, &selection)
        {
            return Some(format!(
                "role '{}' routes to text-only CLI piggyback ({}:{}).\n\
                 /execute needs tool-capable backends for scout/synth/orchestrate.",
                scope.role, selection.instance_id, selection.model
            ));
        }
    }
    None
}

/// Run the aden task partition sequentially: one pump per sub-scope, dense merge.
async fn execute_partition_sequential(
    dir: &Path,
    caps: &aden::AdenCaps,
    sel: &ModelSel,
    bwrap: bool,
    resume: bool,
    mut progress: ExecuteProgress<'_>,
) -> String {
    let Some((name, seeds, budget)) = task_config() else {
        return "set COXN_TASK_NAME + COXN_TASK_SEEDS first (see /scope)".to_string();
    };
    if !caps.available {
        return "aden required for /execute".to_string();
    }
    if sel.is_offline_stub() {
        return "no model reachable — start a provider before /execute".to_string();
    }
    let index = match aden::scope_agents(dir, &name, &seeds, budget) {
        Ok(i) => i,
        Err(e) => return format!("aden scope --agents failed: {e}"),
    };
    let scopes = agents::parse_index(&index);
    let ordered = agents::dependency_order(&scopes);
    if ordered.is_empty() {
        return "aden returned no sub-scopes".to_string();
    }
    if let Some(msg) = execute_route_guard(dir, caps, sel, &ordered) {
        return msg;
    }
    let resume_slug = resume.then(|| run_ledger::latest_for_task(&name)).flatten();
    let prior_statuses = resume_slug
        .as_deref()
        .map(run_ledger::scope_statuses)
        .unwrap_or_default();
    let mut ledger = match &resume_slug {
        Some(slug) => run_ledger::RunLedger::open(slug),
        None => run_ledger::RunLedger::create(&name),
    };
    ledger.append(
        if resume_slug.is_some() {
            "run_resumed"
        } else {
            "run_started"
        },
        None,
        None,
        json!({ "task": name, "scope_count": ordered.len() }),
    );
    let mut upstream = String::new();
    let mut report = format!(
        "{} partition '{name}' ({} scopes, run {})…\n",
        if resume_slug.is_some() {
            "resuming"
        } else {
            "executing"
        },
        ordered.len(),
        ledger.run()
    );
    progress.emit(&report);
    let mut run_status = "success";
    for (i, scope) in ordered.iter().enumerate() {
        if progress.cancelled() {
            append_report(&mut report, &mut progress, "\n  ✗ — cancelled (Ctrl-C)\n");
            run_status = "cancelled";
            break;
        }
        if let Some(prior) = prior_statuses.get(&scope.id)
            && prior.status == "success"
        {
            append_report(
                &mut report,
                &mut progress,
                &format!(
                    "  ✓ [{}/{}] {} ({}) — skipped (complete)\n",
                    i + 1,
                    ordered.len(),
                    scope.id,
                    scope.role
                ),
            );
            if !prior.result.is_empty() {
                upstream.push_str(&format!("\n--- {} ---\n{}\n", scope.id, prior.result));
            }
            continue;
        }
        let cfg = provider::load_config(dir);
        let (model, sub_sel) = match resolve_role(dir, caps, &scope.role) {
            Some(selection) => match resolve_instance_from_config(&cfg, selection, "route") {
                Some(resolved) => resolved,
                None => {
                    return abort_report(
                        &mut report,
                        &mut progress,
                        &format!("provider unavailable for role '{}'", scope.role),
                    );
                }
            },
            None => match &sel.endpoint {
                Some(e) => openai_model(
                    e.instance_id.clone(),
                    e.base_url.clone(),
                    sel.name.clone(),
                    e.key.clone(),
                    e.source.clone(),
                ),
                None => {
                    return abort_report(&mut report, &mut progress, "no provider for sub-agent");
                }
            },
        };
        let policy = ToolPolicy::for_role(&scope.role);
        let tools = registry_for_policy(dir, caps, bwrap, policy);
        let manifest_path = dir.join(&scope.manifest);
        ledger.append(
            "scope_started",
            Some(&scope.id),
            Some(&scope.role),
            json!({
                "manifest": scope.manifest,
                "policy": policy.label(),
                "depends_on": scope.depends_on,
            }),
        );
        ledger.append(
            "model_selected",
            Some(&scope.id),
            Some(&scope.role),
            json!({ "label": sub_sel.label() }),
        );
        let gate: Box<dyn gate::Gate> = Box::new(aden::AdenGate::new(
            dir.to_path_buf(),
            manifest_path.clone(),
        ));
        let scope_budget = aden::budget_from_manifest(&manifest_path)
            .ok()
            .flatten()
            .unwrap_or(budget);
        append_report(
            &mut report,
            &mut progress,
            &format!(
                "  ⟳ [{}/{}] {} ({}) — running…\n",
                i + 1,
                ordered.len(),
                scope.id,
                scope.role
            ),
        );
        let mut context = AGENT_PREAMBLE_BASE.to_string();
        context.push_str(AGENT_PREAMBLE_ADEN);
        context.push_str(&format!(
            "\n=== sub-agent budget ===\nRequested budget: {scope_budget} tokens. Treat this as a hard context and output budget.\n"
        ));
        if let Ok(manifest_seeds) = aden::seeds_from_manifest(&manifest_path) {
            for s in &manifest_seeds {
                if let Ok(text) = aden::pull(dir, aden::Pull::Asm(s)) {
                    context.push_str(&text);
                    context.push('\n');
                }
            }
        }
        if !upstream.is_empty() {
            context.push_str("\n=== upstream agent results ===\n");
            context.push_str(&upstream);
        }
        let mut pump = Pump::new(model, tools);
        pump.set_gate(gate);
        // Per-sub-agent turn cap: tighter than the global hop cap so a stalling
        // scope returns a dense result to its dependents instead of grinding the
        // full budget. `COXN_SUBAGENT_MAX_TURNS` overrides (0/empty = default).
        let sub_cap = std::env::var("COXN_SUBAGENT_MAX_TURNS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|c| *c > 0);
        pump.set_max_turns(sub_cap);
        pump.set_context(context);
        pump.push_user(format!(
            "Complete sub-scope '{}' (role: {}). Work only within your file mandate. \
             Return a dense summary of actions and findings.",
            scope.id, scope.role
        ));
        let mut batch = BatchIo::new();
        let mut io = run_ledger::LedgerTurnIo::new(&mut batch, &mut ledger, &scope.id, &scope.role);
        match pump.run_turn_streaming(&mut io).await {
            Ok(_) => {
                let result = batch.result();
                let usage = pump
                    .last_usage()
                    .map(|u| {
                        format!(
                            ", budget {}, usage p{} c{} t{}",
                            scope_budget, u.prompt_tokens, u.completion_tokens, u.total_tokens
                        )
                    })
                    .unwrap_or_else(|| format!(", budget {scope_budget}"));
                append_report(
                    &mut report,
                    &mut progress,
                    &format!(
                        "  ✓ [{}/{}] {} ({}, {}, {}{}) — done\n",
                        i + 1,
                        ordered.len(),
                        scope.id,
                        scope.role,
                        policy.label(),
                        sub_sel.label(),
                        usage
                    ),
                );
                upstream.push_str(&format!("\n--- {} ---\n{}\n", scope.id, result));
                ledger.append(
                    "scope_finished",
                    Some(&scope.id),
                    Some(&scope.role),
                    json!({
                        "status": "success",
                        "result_chars": result.chars().count(),
                        "result": result,
                    }),
                );
            }
            Err(e) => {
                append_report(
                    &mut report,
                    &mut progress,
                    &format!(
                        "  ✗ [{}/{}] {} ({}) — error: {e}\n",
                        i + 1,
                        ordered.len(),
                        scope.id,
                        scope.role
                    ),
                );
                run_status = "error";
                ledger.append(
                    "scope_finished",
                    Some(&scope.id),
                    Some(&scope.role),
                    json!({ "status": "error", "error": e.to_string() }),
                );
                break;
            }
        }
    }
    ledger.append("run_finished", None, None, json!({ "status": run_status }));
    append_report(&mut report, &mut progress, "\n=== merged upstream ===\n");
    append_report(&mut report, &mut progress, &upstream);
    report
}
// === Parallel wave scheduler (COXN_EXECUTE_JOBS > 1) ============================
//
// Correctness invariant: the aden gate judges the WHOLE working-tree diff, so
// two concurrently-editing scopes would each see the other's edit and false-block.
// Parallel execution therefore only ever runs READ-ONLY scopes concurrently
// (their pump never invokes the gate); mutating scopes serialize on this thread.
// Read-only aden tool subprocesses run with ADEN_SKIP_AUTO_GEN and are safe to
// overlap. The default (`jobs <= 1`) takes the proven sequential path unchanged.

/// A pre-resolved, owned, Send-safe bundle describing one sub-scope to run on a
/// worker thread. The non-Send `ToolRegistry`/`Gate`/`Pump` are built INSIDE the
/// worker (never crossing the thread boundary), so only Send data is moved in.
struct ScopeInput {
    workdir: PathBuf,
    available: bool,
    bwrap: bool,
    scope_id: String,
    role: String,
    manifest_path: PathBuf,
    budget: u64,
    context: String,
    model: AnyModel,
    label: String,
}

/// What one sub-scope produced, returned to the driving thread for ledger merge.
struct ScopeOutcome {
    ok: bool,
    result: String,
    error: String,
    usage: Option<Usage>,
    label: String,
    policy_label: &'static str,
    budget: u64,
    parallel: bool,
}

impl ScopeOutcome {
    fn err(policy_label: &'static str, label: &str, role: &str, msg: impl Into<String>) -> Self {
        let _ = role;
        Self {
            ok: false,
            result: String::new(),
            error: msg.into(),
            usage: None,
            label: label.to_string(),
            policy_label,
            budget: 0,
            parallel: false,
        }
    }
}

/// Resolve the model + status-line label for one sub-scope on the driving thread
/// (provider config reads never run on workers). `None` means the role/profile
/// gives no usable backend.
fn resolve_sub_model(
    dir: &Path,
    caps: &aden::AdenCaps,
    sel: &ModelSel,
    role: &str,
) -> Option<(AnyModel, String)> {
    let cfg = provider::load_config(dir);
    match resolve_role(dir, caps, role) {
        Some(selection) => resolve_instance_from_config(&cfg, selection, "route")
            .map(|(model, sub_sel)| (model, sub_sel.label())),
        None => sel.endpoint.as_ref().map(|e| {
            let (model, sub_sel) = openai_model(
                e.instance_id.clone(),
                e.base_url.clone(),
                sel.name.clone(),
                e.key.clone(),
                e.source.clone(),
            );
            (model, sub_sel.label())
        }),
    }
}

/// Assemble the per-scope system prompt: base+aden preambles, the budget nudge,
/// the manifest's anchor `asm` context, and dense upstream results. Runs on the
/// driving thread so aden `asm` subprocesses never overlap a worker.
fn assemble_context(dir: &Path, manifest_path: &Path, scope_budget: u64, upstream: &str) -> String {
    let mut context = AGENT_PREAMBLE_BASE.to_string();
    context.push_str(AGENT_PREAMBLE_ADEN);
    context.push_str(&format!(
        "\n=== sub-agent budget ===\nRequested budget: {scope_budget} tokens. \
         Treat this as a hard context and output budget.\n"
    ));
    if let Ok(manifest_seeds) = aden::seeds_from_manifest(manifest_path) {
        for s in &manifest_seeds {
            if let Ok(text) = aden::pull(dir, aden::Pull::Asm(s)) {
                context.push_str(&text);
                context.push('\n');
            }
        }
    }
    if !upstream.is_empty() {
        context.push_str("\n=== upstream agent results ===\n");
        context.push_str(upstream);
    }
    context
}

/// The resolved budget for a scope: the manifest's `context.budget`, falling back
/// to the task-level budget.
fn scope_budget(dir: &Path, manifest: &str, fallback: u64) -> u64 {
    aden::budget_from_manifest(&dir.join(manifest))
        .ok()
        .flatten()
        .unwrap_or(fallback)
}

/// True when no two scopes in the wave may touch the same file. Read-only scopes'
/// mandates are disjoint by construction only if their `files` lists don't
/// overlap; recorded as a diagnostic (read-only scopes never mutate, so a
/// non-disjoint set is still safe -- it merely means shared aden context anchors).
fn mandates_disjoint(dir: &Path, ordered: &[&agents::SubScope], indices: &[usize]) -> bool {
    let mut seen: std::collections::HashSet<String> = HashSet::new();
    for &i in indices {
        let manifest_path = dir.join(&ordered[i].manifest);
        let Ok(files) = aden::files_from_manifest(&manifest_path) else {
            return false;
        };
        for f in files {
            if !seen.insert(f) {
                return false;
            }
        }
    }
    true
}

/// Build the per-scope pump and run one turn, returning the dense result + usage.
/// Runs on the driving thread (mutating scopes) or a worker thread (read-only
/// scopes). The future holds non-Send pump state, so callers block-on it locally
/// (`current_thread` runtime per worker); it is never `spawn`ed across threads.
async fn run_one_scope(input: ScopeInput, parallel: bool) -> ScopeOutcome {
    let policy = ToolPolicy::for_role(&input.role);
    let caps_local = aden::AdenCaps {
        available: input.available,
        model_base_url: None,
        model_name: None,
    };
    let tools = registry_for_policy(&input.workdir, &caps_local, input.bwrap, policy);
    let gate: Box<dyn gate::Gate> = Box::new(aden::AdenGate::new(
        input.workdir.clone(),
        input.manifest_path.clone(),
    ));
    let mut pump = Pump::new(input.model, tools);
    pump.set_gate(gate);
    let sub_cap = std::env::var("COXN_SUBAGENT_MAX_TURNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|c| *c > 0);
    pump.set_max_turns(sub_cap);
    pump.set_context(input.context);
    pump.push_user(format!(
        "Complete sub-scope '{}' (role: {}). Work only within your file mandate. \
         Return a dense summary of actions and findings.",
        input.scope_id, input.role
    ));
    let mut io = BatchIo::new();
    match pump.run_turn_streaming(&mut io).await {
        Ok(_) => ScopeOutcome {
            ok: true,
            result: io.result(),
            error: String::new(),
            usage: pump.last_usage(),
            label: input.label,
            policy_label: policy.label(),
            budget: input.budget,
            parallel,
        },
        Err(e) => ScopeOutcome::err(
            policy.label(),
            &input.label,
            &input.role,
            format!("error: {e}"),
        ),
    }
}

/// Prepare inputs for one scope: resolve model + assemble context + build the
/// Send-safe `ScopeInput`. Driving-thread-only (reads provider config, runs aden
/// `asm`). Returns `None` only on model-resolution failure.
fn prepare_scope(
    dir: &Path,
    caps: &aden::AdenCaps,
    sel: &ModelSel,
    bwrap: bool,
    scope: &agents::SubScope,
    budget: u64,
    upstream: &str,
) -> Result<ScopeInput, ScopeOutcome> {
    let manifest_path = dir.join(&scope.manifest);
    let Some((model, label)) = resolve_sub_model(dir, caps, sel, &scope.role) else {
        return Err(ScopeOutcome::err(
            ToolPolicy::for_role(&scope.role).label(),
            &scope.role,
            &scope.role,
            format!("provider unavailable for role '{}'", scope.role),
        ));
    };
    let b = scope_budget(dir, &scope.manifest, budget);
    let context = assemble_context(dir, &manifest_path, b, upstream);
    Ok(ScopeInput {
        workdir: dir.to_path_buf(),
        available: caps.available,
        bwrap,
        scope_id: scope.id.clone(),
        role: scope.role.clone(),
        manifest_path,
        budget: b,
        context,
        model,
        label,
    })
}

/// Run a single scope on the driving thread via a throwaway current_thread runtime.
fn run_scope_on_main(input: ScopeInput, parallel: bool) -> ScopeOutcome {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("per-scope runtime");
    rt.block_on(run_one_scope(input, parallel))
}

/// Shared, borrowed context for one dependency wave of the parallel scheduler
/// (bundles the many driver-thread references so worker helpers stay narrow).
struct WaveCtx<'a> {
    dir: &'a Path,
    caps: &'a aden::AdenCaps,
    sel: &'a ModelSel,
    bwrap: bool,
    ordered: &'a [&'a agents::SubScope],
    budget: u64,
    upstream: &'a str,
    jobs: usize,
}

/// Run a set of scopes concurrently on worker threads (capped at `jobs`), in
/// batches. Only ever called with READ-ONLY scopes (mutating scopes serialize on
/// the driving thread). Returns outcomes for every scope in `indices`, in order.
fn run_read_only_wave(ctx: &WaveCtx<'_>, indices: &[usize]) -> Vec<ScopeOutcome> {
    let mut outcomes: Vec<Option<ScopeOutcome>> = (0..indices.len()).map(|_| None).collect();
    // Process in batches of `jobs` so the worker pool is bounded.
    for batch in indices.chunks(ctx.jobs.max(1)) {
        let mut handles = Vec::new();
        for (slot, &idx) in batch.iter().enumerate() {
            let global_slot = indices.iter().position(|i| *i == idx).unwrap_or(slot);
            match prepare_scope(
                ctx.dir,
                ctx.caps,
                ctx.sel,
                ctx.bwrap,
                ctx.ordered[idx],
                ctx.budget,
                ctx.upstream,
            ) {
                Ok(input) => {
                    let handle = std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .build()
                            .expect("worker runtime");
                        rt.block_on(run_one_scope(input, true))
                    });
                    handles.push((global_slot, handle));
                }
                Err(outcome) => {
                    outcomes[global_slot] = Some(outcome);
                }
            }
        }
        for (slot, handle) in handles {
            let scope = ctx.ordered[indices[slot]];
            outcomes[slot] = Some(handle.join().unwrap_or_else(|_| {
                ScopeOutcome::err(
                    ToolPolicy::for_role(&scope.role).label(),
                    &scope.role,
                    &scope.role,
                    "sub-agent worker thread panicked",
                )
            }));
        }
    }
    outcomes.into_iter().map(|o| o.expect("filled")).collect()
}

/// The parallel wave runner. Emits the same coarse ledger events as the
/// sequential path (run_started/scope_started/model_selected/scope_finished/
/// run_finished); granular per-tool events are omitted for the opt-in parallel
/// mode (the tradeoff documented in PLAN and routing docs).
#[allow(clippy::too_many_lines)]
async fn execute_partition_parallel(
    dir: &Path,
    caps: &aden::AdenCaps,
    sel: &ModelSel,
    bwrap: bool,
    jobs: usize,
    mut progress: ExecuteProgress<'_>,
) -> String {
    let Some((name, seeds, budget)) = task_config() else {
        return "set COXN_TASK_NAME + COXN_TASK_SEEDS first (see /scope)".to_string();
    };
    if !caps.available {
        return "aden required for /execute".to_string();
    }
    if sel.is_offline_stub() {
        return "no model reachable — start a provider before /execute".to_string();
    }
    let index = match aden::scope_agents(dir, &name, &seeds, budget) {
        Ok(i) => i,
        Err(e) => return format!("aden scope --agents failed: {e}"),
    };
    let scopes = agents::parse_index(&index);
    let ordered = agents::dependency_order(&scopes);
    if ordered.is_empty() {
        return "aden returned no sub-scopes".to_string();
    }
    if let Some(msg) = execute_route_guard(dir, caps, sel, &ordered) {
        return msg;
    }
    let mut ledger = run_ledger::RunLedger::create(&name);
    // Record whether every read-only scope wave can run disjoint by mandate.
    // Read-only scopes never mutate, so this is informational, not a gate.
    let all_ro: Vec<usize> = (0..ordered.len())
        .filter(|&i| ToolPolicy::for_role(&ordered[i].role) == ToolPolicy::ReadOnly)
        .collect();
    let disjoint = mandates_disjoint(dir, &ordered, &all_ro);
    ledger.append(
        "run_started",
        None,
        None,
        json!({ "task": name, "scope_count": ordered.len(), "jobs": jobs, "read_only_disjoint": disjoint }),
    );
    let mut report = format!(
        "executing partition '{name}' ({} scopes, run {}, jobs {})…\n",
        ordered.len(),
        ledger.run(),
        jobs
    );
    progress.emit(&report);
    let mut upstream = String::new();
    let mut completed: HashSet<String> = HashSet::new();
    let mut failed_ids: HashSet<String> = HashSet::new();
    let mut run_status = "success";
    let mut reported = 0usize;
    let total = ordered.len();

    let mut pending: Vec<usize> = (0..ordered.len()).collect();
    while !pending.is_empty() {
        if progress.cancelled() {
            append_report(&mut report, &mut progress, "\n  ✗ — cancelled (Ctrl-C)\n");
            run_status = "cancelled";
            break;
        }
        // Ready: deps all complete and this scope never tried-and-failed.
        let ready: Vec<usize> = pending
            .iter()
            .copied()
            .filter(|&i| {
                !failed_ids.contains(&ordered[i].id)
                    && ordered[i].depends_on.iter().all(|d| completed.contains(d))
            })
            .collect();
        if ready.is_empty() {
            // Everything left is blocked by a failed dependency.
            for &i in &pending {
                run_status = "error";
                reported += 1;
                append_report(
                    &mut report,
                    &mut progress,
                    &format!(
                        "  ✗ [{reported}/{total}] {} ({}) — blocked by an unmet dependency\n",
                        ordered[i].id, ordered[i].role
                    ),
                );
            }
            break;
        }
        // Split ready into read-only (parallel-safe) and mutating (serialize).
        let read_only: Vec<usize> = ready
            .iter()
            .copied()
            .filter(|&i| ToolPolicy::for_role(&ordered[i].role) == ToolPolicy::ReadOnly)
            .collect();
        let mutating: Vec<usize> = ready
            .iter()
            .copied()
            .filter(|&i| ToolPolicy::for_role(&ordered[i].role) != ToolPolicy::ReadOnly)
            .collect();

        // Collect outcomes per ready scope, keyed by index, in dependency order.
        let mut outcomes: Vec<(usize, ScopeOutcome)> = Vec::with_capacity(ready.len());

        if read_only.len() > 1 {
            // Run read-only ready scopes concurrently (bounded by `jobs`).
            let ctx = WaveCtx {
                dir,
                caps,
                sel,
                bwrap,
                ordered: &ordered,
                budget,
                upstream: &upstream,
                jobs,
            };
            let ro_outcomes = run_read_only_wave(&ctx, &read_only);
            for (k, o) in ro_outcomes.into_iter().enumerate() {
                outcomes.push((read_only[k], o));
            }
        } else {
            // Zero or one read-only scope: run it on the driving thread.
            for &i in &read_only {
                match prepare_scope(dir, caps, sel, bwrap, ordered[i], budget, &upstream) {
                    Ok(input) => outcomes.push((i, run_scope_on_main(input, false))),
                    Err(o) => outcomes.push((i, o)),
                }
            }
        }
        // Mutating scopes always run sequentially on the driving thread.
        for &i in &mutating {
            match prepare_scope(dir, caps, sel, bwrap, ordered[i], budget, &upstream) {
                Ok(input) => outcomes.push((i, run_scope_on_main(input, false))),
                Err(o) => outcomes.push((i, o)),
            }
        }

        // Report + ledger in dependency order (stable).
        outcomes.sort_by_key(|(i, _)| *i);
        for (idx, outcome) in outcomes {
            let scope = &ordered[idx];
            reported += 1;
            ledger.append(
                "scope_started",
                Some(&scope.id),
                Some(&scope.role),
                json!({
                    "manifest": scope.manifest,
                    "policy": outcome.policy_label,
                    "depends_on": scope.depends_on,
                    "parallel": outcome.parallel,
                }),
            );
            ledger.append(
                "model_selected",
                Some(&scope.id),
                Some(&scope.role),
                json!({ "label": outcome.label }),
            );
            if outcome.ok {
                let usage = outcome
                    .usage
                    .map(|u| {
                        format!(
                            ", budget {}, usage p{} c{} t{}",
                            outcome.budget, u.prompt_tokens, u.completion_tokens, u.total_tokens
                        )
                    })
                    .unwrap_or_else(|| format!(", budget {}", outcome.budget));
                append_report(
                    &mut report,
                    &mut progress,
                    &format!(
                        "  ✓ [{reported}/{total}] {} ({}, {}, {}){} — done\n",
                        scope.id, scope.role, outcome.policy_label, outcome.label, usage
                    ),
                );
                upstream.push_str(&format!("\n--- {} ---\n{}\n", scope.id, outcome.result));
                completed.insert(scope.id.clone());
                ledger.append(
                    "scope_finished",
                    Some(&scope.id),
                    Some(&scope.role),
                    json!({
                        "status": "success",
                        "result_chars": outcome.result.chars().count(),
                    }),
                );
            } else {
                run_status = "error";
                failed_ids.insert(scope.id.clone());
                append_report(
                    &mut report,
                    &mut progress,
                    &format!(
                        "  ✗ [{reported}/{total}] {id} ({role}) — error: {err}\n",
                        id = scope.id,
                        role = scope.role,
                        err = outcome.error
                    ),
                );
                ledger.append(
                    "scope_finished",
                    Some(&scope.id),
                    Some(&scope.role),
                    json!({ "status": "error", "error": outcome.error }),
                );
            }
        }
        // Drop completed and failed scopes; dependents stay until they become
        // ready (or are reported as blocked when no progress is possible).
        pending.retain(|i| {
            let id = &ordered[*i].id;
            !completed.contains(id) && !failed_ids.contains(id)
        });
    }

    ledger.append("run_finished", None, None, json!({ "status": run_status }));
    append_report(&mut report, &mut progress, "\n=== merged upstream ===\n");
    append_report(&mut report, &mut progress, &upstream);
    report
}
#[cfg(test)]
mod tests {
    use super::*;

    fn advertised_names(registry: &ToolRegistry) -> Vec<String> {
        registry
            .advertised_defs()
            .into_iter()
            .map(|d| d.name)
            .collect()
    }

    #[test]
    fn role_policy_maps_known_roles() {
        assert_eq!(ToolPolicy::for_role("scout"), ToolPolicy::ReadOnly);
        assert_eq!(ToolPolicy::for_role("synth"), ToolPolicy::Edit);
        assert_eq!(ToolPolicy::for_role("orchestrate"), ToolPolicy::Command);
        assert_eq!(ToolPolicy::for_role("custom"), ToolPolicy::Full);
    }

    #[test]
    fn scout_policy_cannot_mutate() {
        let caps = aden::AdenCaps {
            available: false,
            model_base_url: None,
            model_name: None,
        };
        let registry = registry_for_policy(Path::new("."), &caps, false, ToolPolicy::ReadOnly);
        let names = advertised_names(&registry);
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"grep".to_string()));
        assert!(names.contains(&"glob".to_string()));
        assert!(!names.contains(&"edit".to_string()));
        assert!(!names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"run_command".to_string()));
    }

    #[test]
    fn execute_progress_cancel_callback() {
        let mut snapshots = Vec::new();
        let mut capture = |r: &str| snapshots.push(r.to_string());
        let mut cancel = || true;
        let mut progress = ExecuteProgress::new(&mut capture, Some(&mut cancel));
        let mut report = "start\n".to_string();
        progress.emit(&report);
        assert!(progress.cancelled());
        append_report(&mut report, &mut progress, "  ✗ — cancelled\n");
        assert!(snapshots[1].contains("cancelled"));
    }

    #[test]
    fn append_report_emits_on_every_append() {
        let mut snapshots = Vec::new();
        let mut capture = |r: &str| snapshots.push(r.to_string());
        let mut progress = ExecuteProgress::new(&mut capture, None);
        let mut report = "executing partition 't' (2 scopes)…\n".to_string();
        progress.emit(&report);
        append_report(&mut report, &mut progress, "  ⟳ [1/2] scout — running…\n");
        append_report(&mut report, &mut progress, "  ✓ [1/2] scout — done\n");
        assert_eq!(snapshots.len(), 3);
        assert!(snapshots[2].contains("✓ [1/2] scout"));
        assert_eq!(report, snapshots[2]);
    }

    #[test]
    fn execute_jobs_defaults_and_clamps() {
        // SAFETY: COXN_EXECUTE_JOBS is read only by execute_partition's scheduler
        // path, which is not exercised by any concurrent test; this test is the
        // sole reader/writer. Process-global env still, so it runs serialized
        // by the harness's single test-thread here is NOT guaranteed -- but no
        // other test reads this var.
        unsafe {
            std::env::remove_var("COXN_EXECUTE_JOBS");
        }
        assert_eq!(execute_jobs(), 1);
        unsafe {
            std::env::set_var("COXN_EXECUTE_JOBS", "3");
        }
        assert_eq!(execute_jobs(), 3);
        unsafe {
            std::env::set_var("COXN_EXECUTE_JOBS", "64");
        }
        assert_eq!(execute_jobs(), 8);
        unsafe {
            std::env::set_var("COXN_EXECUTE_JOBS", "0");
        }
        assert_eq!(execute_jobs(), 1);
        unsafe {
            std::env::set_var("COXN_EXECUTE_JOBS", "nope");
        }
        assert_eq!(execute_jobs(), 1);
        unsafe {
            std::env::remove_var("COXN_EXECUTE_JOBS");
        }
    }

    #[test]
    fn execute_route_guard_blocks_text_only_active_model() {
        let caps = aden::AdenCaps {
            available: false,
            model_base_url: None,
            model_name: None,
        };
        let sel = ModelSel {
            name: "m".into(),
            endpoint: Some(crate::app::Endpoint {
                instance_id: "grok".into(),
                base_url: format!("{}grok", crate::grok_cli::GROK_CLI_SCHEME),
                key: None,
                source: "test".into(),
            }),
        };
        let msg = execute_route_guard(Path::new("."), &caps, &sel, &[]).expect("should block");
        assert!(msg.contains("text-only"));
    }

    #[test]
    fn execute_route_guard_blocks_text_only_role_route() {
        let dir = std::env::temp_dir().join(format!("coxn-exec-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".aden")).unwrap();
        std::fs::write(
            dir.join(".aden/config.toml"),
            r#"
[provider.local]
driver = "openai_compat"
base_url = "http://localhost:11434/v1"
enabled = true

[provider.grok]
driver = "grok_cli"
binary = "grok"
enabled = true

[route]
active = "local:llama3"
scout = "grok:grok-model"
"#,
        )
        .unwrap();
        let caps = aden::AdenCaps {
            available: false,
            model_base_url: None,
            model_name: None,
        };
        let sel = ModelSel {
            name: "llama3".into(),
            endpoint: Some(crate::app::Endpoint {
                instance_id: "local".into(),
                base_url: "http://localhost:11434/v1".into(),
                key: None,
                source: "test".into(),
            }),
        };
        let scope = agents::SubScope {
            id: "t-0".into(),
            role: "scout".into(),
            manifest: "manifest.json".into(),
            depends_on: vec![],
        };
        let ordered: Vec<&agents::SubScope> = vec![&scope];
        let msg =
            execute_route_guard(&dir, &caps, &sel, &ordered).expect("should block scout route");
        assert!(msg.contains("scout"));
        assert!(msg.contains("grok:grok-model"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn execute_partition_resolves_distinct_role_routes_without_aden() {
        let dir = std::env::temp_dir().join(format!("coxn-exec-role-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".aden")).unwrap();
        std::fs::write(
            dir.join(".aden/config.toml"),
            r#"
[provider.scout]
driver = "stub"
enabled = true

[provider.synth]
driver = "stub"
enabled = true

[route]
scout = "scout:scout-model"
synth = "synth:synth-model"
active = "scout:scout-model"
"#,
        )
        .unwrap();
        let caps = aden::AdenCaps {
            available: false,
            model_base_url: None,
            model_name: None,
        };
        let cfg = provider::load_config(&dir);
        let scout = resolve_role(&dir, &caps, "scout").expect("scout route");
        let synth = resolve_role(&dir, &caps, "synth").expect("synth route");
        assert_eq!(scout.instance_id, "scout");
        assert_eq!(scout.model, "scout-model");
        assert_eq!(synth.instance_id, "synth");
        assert_eq!(synth.model, "synth-model");
        assert!(resolve_instance_from_config(&cfg, scout, "route").is_some());
        assert!(resolve_instance_from_config(&cfg, synth, "route").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mandates_disjoint_detects_overlap() {
        let dir = std::env::temp_dir();
        // Two manifests with disjoint files => disjoint.
        let m_a = dir.join(format!("coxn-disjoint-a-{}.json", std::process::id()));
        let m_b = dir.join(format!("coxn-disjoint-b-{}.json", std::process::id()));
        std::fs::write(&m_a, r#"{"name":"a","files":["src/a.rs"]}"#).unwrap();
        std::fs::write(&m_b, r#"{"name":"b","files":["src/b.rs","src/c.rs"]}"#).unwrap();
        let scope_a = agents::SubScope {
            id: "a".into(),
            role: "scout".into(),
            manifest: m_a.to_str().unwrap().to_string(),
            depends_on: vec![],
        };
        let scope_b = agents::SubScope {
            id: "b".into(),
            role: "scout".into(),
            manifest: m_b.to_str().unwrap().to_string(),
            depends_on: vec![],
        };
        let ordered: Vec<&agents::SubScope> = vec![&scope_a, &scope_b];
        assert!(mandates_disjoint(&dir, &ordered, &[0, 1]));

        // Overlapping files => not disjoint.
        std::fs::write(&m_b, r#"{"name":"b","files":["src/a.rs","src/c.rs"]}"#).unwrap();
        assert!(!mandates_disjoint(&dir, &ordered, &[0, 1]));

        // Missing/unreadable mandate => conservative false.
        std::fs::remove_file(&m_b).unwrap();
        assert!(!mandates_disjoint(&dir, &ordered, &[0, 1]));
        std::fs::remove_file(&m_a).ok();
    }
}
