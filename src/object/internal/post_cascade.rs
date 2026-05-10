//! Post-MyLevel script-driven asset-load discovery.
//!
//! UE2's `LoadMap` reads MyLevel synchronously, then runs game-init code
//! that executes UnrealScript bytecode for each actor (`PreBeginPlay`,
//! `BeginPlay`, `PostBeginPlay`, `SetInitialState`). Those scripts call
//! `StaticLoadObject` / `DynamicLoadObject` with literal asset names;
//! the engine reads each named object's bytes at the underlying `.lin`
//! cursor at the moment of the call. Because the LIN compactor lays
//! bytes in linear engine-call order, the cursor advances correctly as
//! each load fires.
//!
//! ## What this walker actually does (and doesn't)
//!
//! This is a **pattern-matching bytecode scanner, not an interpreter**.
//! It does not execute scripts. It works by:
//!
//! 1. Iterating the parsed `Expr` tree in source order.
//! 2. Recognizing the specific call shape `Token(FinalFunction)
//!    Object(idx) ...args... EndFunctionParms` where `idx` resolves to
//!    a function named `DynamicLoadObject` or `StaticLoadObject`.
//! 3. Extracting the first `StringConst` payload from those calls and
//!    calling `load_object_by_full_name` with it.
//! 4. Recognizing one specific guard pattern — `if (instance_var ==
//!    None)` compiled as `JumpIfNot Word(N) Native(119)
//!    InstanceVariable Object NoObject EndFunctionParms` — and
//!    skipping the conditional body when the actor's class CDO has a
//!    non-null default for that variable.
//!
//! ## Known limits vs. engine semantics
//!
//! - Flat iteration assumes straight-line code. Branches/loops other
//!   than the recognized `if (X == None)` pattern are walked as if
//!   their bytecode unconditionally executes, which can over-trigger
//!   loads relative to the engine.
//! - Only `DynamicLoadObject` / `StaticLoadObject` are recognized as
//!   load callsites. Loads through helper/wrapper functions or
//!   delegates are missed.
//! - The `if (X == None)` guard checks only class-CDO defaults; loads
//!   gated by per-instance property tags (`Mesh = SkeletalMesh'X'`
//!   set in the level editor on a specific actor) are not gated
//!   because we don't parse actor instance bodies past the
//!   variable-length `HAS_STACK` state frame.
//! - The walker iterates each actor's class chain; the engine instead
//!   resolves a single function via `FindFunctionChecked` and
//!   `ProcessEvent`s it once per actor. For shared base classes our
//!   walker visits the per-class bytecode once across the whole map,
//!   the engine runs it once per instance.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io;
use std::rc::Rc;

/// Pointer-pair used as a cycle key for `walk_class_chain`. Hashing via
/// the raw vtable thin pointer alone would dedupe shared-base classes
/// across leaves; we want each (class, leaf) combination to be visited
/// once because conditional CDO checks evaluate against the leaf class.
type ClassLeafKey = (*const (), *const ());

use byteorder::ByteOrder;

use crate::de::RcLinker;
use crate::object::internal::script::{Expr, ExprToken};
use crate::object::ufield::Field;
use crate::object::uclass::Class;
use crate::object::ulevel_base::LevelBase;
use crate::object::ustruct::Struct;
use crate::object::{RcUnrealObject, UObjectKind, UnrealObject};
use crate::reader::LinRead;
use crate::runtime::{LoadKind, UnrealRuntime};

/// Object/Class property type byte constants matching `read_tag_value`.
const TAG_TYPE_OBJECT: u8 = 5;
const TAG_TYPE_CLASS: u8 = 8;

/// Trim the trailing null and check shape: ASCII alphanumeric / `_` /
/// `-` segments separated by `.`, at least two non-empty segments. Hyphen
/// is intentional — SC has packages named e.g. `2-1_CIA_tex`.
fn asset_path_from_string_const(bytes: &[u8]) -> Option<String> {
    let trimmed = match bytes.last() {
        Some(0) => &bytes[..bytes.len() - 1],
        _ => bytes,
    };
    if trimmed.is_empty() {
        return None;
    }
    let s = std::str::from_utf8(trimmed).ok()?;
    if !s.contains('.') {
        return None;
    }
    let mut segs = 0;
    for seg in s.split('.') {
        if seg.is_empty() {
            return None;
        }
        for ch in seg.chars() {
            if !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
                return None;
            }
        }
        segs += 1;
    }
    if segs < 2 {
        return None;
    }
    Some(s.to_string())
}

/// Iterate `parsed_script` (a function's bytecode lossless tree) and
/// trigger asset loads for every `StringConst` that shapes up as an
/// asset path. Returns `Err` only when the inner runtime errors out
/// in a way callers can't continue past; "load failed because the
/// resolved name's body bytes don't match the cursor" is a soft signal
/// reported via `Aborted` and propagated up so the surrounding actor
/// walk can stop iterating further classes.
///
/// The walker doesn't track variable values or evaluate control flow.
/// It relies on the property that UE2 bytecode lays `StringConst`
/// operands inline at their source position; flat iteration matches
/// engine execution order for the straight-line code paths NPC init
/// functions use to load animation sets and sound cues.
///
/// Returns `WalkOutcome::Aborted` on the first cursor-misalignment
/// signal (typically a `failed to fill whole buffer` from a body
/// preload that landed in the wrong bytes) so the caller can stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkOutcome {
    Continue,
    Aborted,
}

/// Resolve a `FinalFunction(idx)` callee object name relative to
/// `func_linker` (the linker the bytecode was deserialized from). Returns
/// the function's UObject name (e.g. "DynamicLoadObject") if the callee
/// is already loaded, otherwise None.
fn resolve_callee_name(idx: i32, func_linker: &RcLinker, runtime: &UnrealRuntime) -> Option<String> {
    let obj = runtime.find_loaded_object_by_raw_index(idx, func_linker)?;
    let inner = obj.try_borrow().ok()?;
    let n = inner.base_object().name.clone();
    Some(n)
}

/// In-memory script byte size of one parsed `Expr`. Mirrors UE2's
/// in-memory representation (the engine's `iCode` counter), which is what
/// `JumpIfNot Word(N)` uses as its target offset. Object/Name slots are
/// fixed 4 bytes regardless of the variable-length packed_int they
/// occupied on disk; matches the size accounting in `script.rs`'s
/// `bytes_read`.
fn expr_byte_size(expr: &Expr) -> usize {
    match expr {
        Expr::Token(_) => 1,
        Expr::Native(_) => 1,
        Expr::Object(_) | Expr::Name(_) | Expr::Int(_) | Expr::Float(_) => 4,
        Expr::Byte(_) => 1,
        Expr::Word(_) => 2,
        Expr::Data(b) => b.len(),
    }
}

/// True if any class in the chain rooted at `class_obj` has a CDO tag
/// matching `prop_name` whose property_type is Object or Class. The
/// engine writes such tags only when a default-properties block sets
/// the var to a non-default (non-null) value, so presence of the tag
/// is equivalent to "the runtime initial value of this var is non-null"
/// for the engine's `if (var == None)` check.
fn class_default_overrides_object(class_obj: &RcUnrealObject, prop_name: &str) -> bool {
    let mut current = Some(Rc::clone(class_obj));
    while let Some(cls) = current {
        let next = {
            let Ok(inner) = cls.try_borrow() else {
                break;
            };
            // Inspect this class's default_tags, if any.
            if let Some(class) = inner.as_any().downcast_ref::<Class>() {
                let linker = inner.base_object().linker();
                let l = linker.borrow();
                for tag in &class.default_tags {
                    if !matches!(tag.property_type, TAG_TYPE_OBJECT | TAG_TYPE_CLASS) {
                        continue;
                    }
                    let tag_idx = tag.name.raw() as usize;
                    let tag_name = match l.package.names.get(tag_idx) {
                        Some(n) => n.name.as_str(),
                        None => continue,
                    };
                    if tag_name.eq_ignore_ascii_case(prop_name) {
                        return true;
                    }
                }
                drop(l);
            }
            // Step to super.
            inner
                .parent_of_kind(UObjectKind::Field)
                .and_then(|f| f.as_any().downcast_ref::<Field>())
                .and_then(|f| f.super_field())
        };
        current = next;
    }
    false
}

/// Recognise `Token(JumpIfNot) Word(N) Native(119) Token(InstanceVariable)
/// Object(prop_idx) Token(NoObject) Token(EndFunctionParms)` — UE2's
/// compiled `if (this.X == None)` pattern. Returns
/// `Some((prop_idx, target_byte_offset))` if matched.
fn detect_var_is_none_check(parsed_script: &[Expr], i: usize) -> Option<(i32, u16)> {
    if !matches!(parsed_script.get(i), Some(Expr::Token(ExprToken::JumpIfNot))) {
        return None;
    }
    let Some(Expr::Word(target)) = parsed_script.get(i + 1) else { return None };
    if !matches!(parsed_script.get(i + 2), Some(Expr::Native(119))) {
        return None;
    }
    if !matches!(
        parsed_script.get(i + 3),
        Some(Expr::Token(ExprToken::InstanceVariable))
    ) {
        return None;
    }
    let Some(Expr::Object(prop_idx)) = parsed_script.get(i + 4).cloned() else {
        return None;
    };
    if !matches!(parsed_script.get(i + 5), Some(Expr::Token(ExprToken::NoObject))) {
        return None;
    }
    if !matches!(
        parsed_script.get(i + 6),
        Some(Expr::Token(ExprToken::EndFunctionParms))
    ) {
        return None;
    }
    Some((prop_idx, *target))
}

/// True if `name` is one of the engine's runtime asset-loading natives
/// whose first arg is a `StringConst` with the asset's full path.
/// Compiled by UnrealScript as `FinalFunction(import_to_native_decl)`
/// followed by `StringConst("Pkg.Sub.Name")` and `ObjectConst(class)`,
/// then `EndFunctionParms`. Mirroring this exact call shape avoids the
/// false-positive loads a flat `Data`-iteration walker would trigger.
fn is_asset_load_function(name: &str) -> bool {
    name.eq_ignore_ascii_case("DynamicLoadObject")
        || name.eq_ignore_ascii_case("StaticLoadObject")
}

/// Inside a function call's argument list (i.e. between `FinalFunction`/
/// `Native` and the matching `EndFunctionParms`), find the first
/// `StringConst`'s payload bytes. Returns the index past the
/// `EndFunctionParms` so the outer walker can resume iteration.
fn extract_string_arg(
    parsed_script: &[Expr],
    args_start: usize,
) -> (Option<&[u8]>, usize) {
    let mut found_str: Option<&[u8]> = None;
    let mut i = args_start;
    while i < parsed_script.len() {
        match &parsed_script[i] {
            Expr::Token(ExprToken::EndFunctionParms) => return (found_str, i + 1),
            Expr::Token(ExprToken::StringConst) => {
                if let Some(Expr::Data(bytes)) = parsed_script.get(i + 1)
                    && found_str.is_none()
                {
                    found_str = Some(bytes.as_slice());
                }
            }
            _ => {}
        }
        i += 1;
    }
    (found_str, i)
}

/// Walk a function's parsed bytecode, looking for
/// `Token(FinalFunction) Object(idx) ...args... EndFunctionParms`
/// patterns where `idx` resolves (via `func_linker`) to a function
/// named `DynamicLoadObject` / `StaticLoadObject`. The first
/// `StringConst` arg of each such call is the asset path the engine
/// would resolve at runtime; we trigger the same load here so the
/// cursor advances in lockstep with the engine's runtime call order.
///
/// Recognises UE2's compiled `if (X == None) { ...load... }` pattern
/// (`JumpIfNot Word(N) Native(119) InstanceVariable Object(prop_idx)
/// NoObject EndFunctionParms`) and skips the conditional block when
/// the class CDO has a non-null default for `prop_idx`. The engine
/// evaluates the same condition against the actor's current variable
/// state — which equals the class default at PostBeginPlay time —
/// so a non-null default flips the condition false, and the load
/// inside doesn't fire. Without this we'd trigger the load anyway,
/// fail the body read at the wrong cursor position, and corrupt the
/// stream for subsequent loads.
fn walk_parsed_script<E, R>(
    parsed_script: &[Expr],
    func_linker: &RcLinker,
    owning_class: &RcUnrealObject,
    runtime: &mut UnrealRuntime,
    reader: &mut R,
) -> io::Result<WalkOutcome>
where
    E: ByteOrder,
    R: LinRead,
{
    // Build a parallel byte-offset table so JumpIfNot targets resolve
    // to Vec positions. Offsets are inclusive of the entry at index `i`
    // up to but excluding entry `i+1`, matching the engine's `iCode`
    // accounting.
    let mut byte_at: Vec<usize> = Vec::with_capacity(parsed_script.len() + 1);
    let mut acc = 0usize;
    for e in parsed_script {
        byte_at.push(acc);
        acc += expr_byte_size(e);
    }
    byte_at.push(acc); // sentinel for past-the-end

    let mut i = 0;
    while i < parsed_script.len() {
        // Detect `if (instance_var == None)` — if the var has a non-null
        // default in the class CDO, the engine would skip this block.
        if let Some((prop_idx, target_byte)) = detect_var_is_none_check(parsed_script, i) {
            let prop_name = runtime
                .find_loaded_object_by_raw_index(prop_idx, func_linker)
                .and_then(|p| p.try_borrow().ok().map(|i| i.base_object().name.clone()));
            if let Some(prop_name) = prop_name
                && class_default_overrides_object(owning_class, &prop_name)
            {
                // Class-CDO default for this var is non-null. Skip the
                // conditional block — the engine evaluates `var == None`
                // as false, no body read fires.
                //
                // Note: this catches conditional loads gated by class
                // default overrides only. Loads gated by per-actor
                // instance tags (`Mesh = SkeletalMesh'X'` set in level
                // editor on a specific instance) are NOT caught here —
                // resolving them requires parsing the actor's body tag
                // list, which sits behind a variable-length state frame
                // for HAS_STACK actors. Tracked as a follow-up; a small
                // residual gap (~7 KB / map = ESam class chain + sounds)
                // remains for traced maps that have such instances.
                let new_i = byte_at
                    .iter()
                    .position(|&b| b >= target_byte as usize)
                    .unwrap_or(parsed_script.len());
                tracing::debug!(
                    "post_cascade: skip if-block (prop {prop_name:?} non-null CDO default)"
                );
                i = new_i;
                continue;
            }
        }

        let is_final_call = matches!(parsed_script.get(i), Some(Expr::Token(ExprToken::FinalFunction)));
        if !is_final_call {
            i += 1;
            continue;
        }
        let Some(Expr::Object(callee_idx)) = parsed_script.get(i + 1).cloned() else {
            i += 1;
            continue;
        };
        let callee_name = resolve_callee_name(callee_idx, func_linker, runtime);
        let is_asset_load = callee_name
            .as_deref()
            .map(is_asset_load_function)
            .unwrap_or(false);
        if !is_asset_load {
            i += 2;
            continue;
        }

        // FinalFunction at i, Object(idx) at i+1, args from i+2.
        let (str_arg, after) = extract_string_arg(parsed_script, i + 2);
        i = after;
        let Some(bytes) = str_arg else { continue };
        let Some(name) = asset_path_from_string_const(bytes) else {
            continue;
        };

        let module = name.split('.').next().unwrap_or("");
        let linker_present = runtime
            .linkers
            .keys()
            .any(|k| k.eq_ignore_ascii_case(module));
        if !linker_present {
            tracing::debug!("post_cascade: skip {name:?} (linker {module:?} not loaded)");
            continue;
        }

        runtime.begin_load();
        let result = runtime.load_object_by_full_name::<E, _>(&name, LoadKind::Load, reader);
        if let Err(ref e) = result {
            tracing::debug!("post_cascade: {name} failed: {e}");
        }
        let drain = runtime.end_load::<E, _>(reader);
        match drain {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(
                    "post_cascade: drain after {name} failed ({e}); aborting walk"
                );
                return Ok(WalkOutcome::Aborted);
            }
        }
        if let Ok(Some(_)) = result {
            tracing::debug!("post_cascade: loaded {name}");
        }
    }
    Ok(WalkOutcome::Continue)
}

/// Find a child function by name on a `Struct` (Class/State/Function).
/// Walks `Struct.children` linked list for any descendant that is a
/// `Function` whose UObject `name` matches case-insensitively.
fn find_function_by_name(struct_obj: &RcUnrealObject, name: &str) -> Option<RcUnrealObject> {
    let inner = struct_obj.try_borrow().ok()?;
    let s = inner.parent_of_kind(UObjectKind::Struct)?;
    let s = s.as_any().downcast_ref::<Struct>()?;
    let mut current = s.children.clone();
    drop(inner);
    while let Some(child) = current {
        let Ok(child_inner) = child.try_borrow() else {
            break;
        };
        let is_fn = child_inner.is_a(UObjectKind::Function);
        let cn = child_inner.base_object().name.clone();
        let next = child_inner
            .parent_of_kind(UObjectKind::Field)
            .and_then(|f| f.as_any().downcast_ref::<Field>())
            .and_then(|f| f.next());
        drop(child_inner);
        if is_fn && cn.eq_ignore_ascii_case(name) {
            return Some(child);
        }
        current = next;
    }
    None
}

/// Walk a class chain super-first, calling `walk_parsed_script` on each
/// class's `fn_name` function (if defined). Engine execution order for
/// init functions is super-body then override-body, which is what this
/// produces when the override calls `Super.<fn_name>()` — but we walk
/// super manually too in case the override doesn't call super, since the
/// flat-iteration walker doesn't recurse into `FinalFunction` calls.
/// Cache hits make duplicates harmless.
fn walk_class_chain<E, R>(
    class_obj: &RcUnrealObject,
    leaf_class: &RcUnrealObject,
    fn_names: &[&str],
    visited: &mut HashSet<ClassLeafKey>,
    runtime: &mut UnrealRuntime,
    reader: &mut R,
) -> io::Result<WalkOutcome>
where
    E: ByteOrder,
    R: LinRead,
{
    // Cycle guard keyed by `(class, leaf)`: each combination is walked
    // once because conditional CDO checks evaluate against the leaf
    // class chain, and the same shared base class can produce different
    // outcomes for two different leaves.
    let key: ClassLeafKey = (
        Rc::as_ptr(class_obj) as *const (),
        Rc::as_ptr(leaf_class) as *const (),
    );
    if !visited.insert(key) {
        return Ok(WalkOutcome::Continue);
    }

    // Resolve super-class via Field.super_field on this class. We need
    // to release the borrow before recursing.
    let super_obj = {
        let Ok(inner) = class_obj.try_borrow() else {
            return Ok(WalkOutcome::Continue);
        };
        inner
            .parent_of_kind(UObjectKind::Field)
            .and_then(|f| f.as_any().downcast_ref::<Field>())
            .and_then(|f| f.super_field())
    };
    if let Some(super_obj) = super_obj
        && walk_class_chain::<E, R>(
            &super_obj,
            leaf_class,
            fn_names,
            visited,
            runtime,
            reader,
        )? == WalkOutcome::Aborted
    {
        return Ok(WalkOutcome::Aborted);
    }

    // For each requested function name, find it on this class (only this
    // class — supers handled via the recursion above) and walk its body.
    // Conditional CDO checks use `leaf_class`, not `class_obj`: the
    // engine's `if (var == None)` evaluates against the spawned actor's
    // leaf-class-derived initial state, which inherits non-null defaults
    // from any class in the leaf's super chain — including subclasses
    // below the function-owning class.
    for fn_name in fn_names {
        let Some(func) = find_function_by_name(class_obj, fn_name) else {
            continue;
        };
        let (parsed_script, func_linker) = {
            let Ok(inner) = func.try_borrow() else {
                continue;
            };
            let s = inner.parent_of_kind(UObjectKind::Struct);
            let Some(s) = s else { continue };
            let Some(s) = s.as_any().downcast_ref::<Struct>() else {
                continue;
            };
            (s.parsed_script.clone(), inner.base_object().linker())
        };
        if walk_parsed_script::<E, _>(
            &parsed_script,
            &func_linker,
            leaf_class,
            runtime,
            reader,
        )? == WalkOutcome::Aborted
        {
            return Ok(WalkOutcome::Aborted);
        }
    }
    Ok(WalkOutcome::Continue)
}


/// Resolve the class object for an actor instance via the runtime's
/// class lookup. Each actor's `concrete_obj` was constructed with a
/// `class_index` whose Object lives in some loaded linker; we follow
/// the same `IndexToObject` path the engine would.
fn class_of_actor(
    actor: &RcUnrealObject,
    runtime: &UnrealRuntime,
) -> Option<RcUnrealObject> {
    let inner = actor.try_borrow().ok()?;
    let base = inner.base_object();
    let linker = base.linker();
    let export_idx = base.export_index();
    drop(inner);
    let class_index = {
        let l = linker.borrow();
        l.find_export_by_index(export_idx)?.class_index
    };
    if class_index == 0 {
        return None;
    }
    runtime.find_loaded_object_by_raw_index(class_index, &linker)
}

/// Top-level entry: after the MyLevel cascade ends, walk every actor's
/// init-function bytecode in actor-array order and trigger asset loads
/// for each `StringConst` that shapes up as an asset path. Engine-faithful
/// because it follows the same script-driven `StaticLoadObject` calls the
/// engine executes at the same cursor positions.
pub fn run_post_cascade<E, R>(
    secondary_package: &str,
    runtime: &mut UnrealRuntime,
    reader: &mut R,
) -> io::Result<()>
where
    E: ByteOrder,
    R: LinRead,
{
    // Find the secondary linker (case-insensitive).
    let linker_rc = runtime
        .linkers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(secondary_package))
        .map(|(_, v)| v.clone());
    let Some(linker_rc) = linker_rc else {
        tracing::debug!("post_cascade: secondary linker {secondary_package:?} not loaded");
        return Ok(());
    };

    // Find MyLevel with class=Engine.Level. Some maps (e.g. 2_1_0CIA,
    // 2_1_2CIA) ship two `MyLevel` exports: the real ULevel (with the
    // actors array) and a Package wrapper container. Without the class
    // filter `find_export_by_name` returns the first match — which
    // is the Package wrapper, leaving the actor walk empty and
    // skipping ~17% of session.lin's bytes that the engine reads via
    // the actors' PreBeginPlay/BeginPlay/PostBeginPlay scripts.
    let level_obj = {
        let linker = linker_rc.borrow();
        let Some((level_idx, _)) =
            linker.find_export_by_name_and_class("MyLevel", "Level", "Engine")
        else {
            tracing::debug!(
                "post_cascade: MyLevel export with class=Engine.Level not found"
            );
            return Ok(());
        };
        linker.objects.get(&level_idx).cloned()
    };
    let Some(level_obj) = level_obj else {
        tracing::debug!("post_cascade: MyLevel not constructed");
        return Ok(());
    };

    // Pull out actor refs. LevelBase holds them as `Vec<Option<RcUnrealObject>>`.
    let actors: Vec<RcUnrealObject> = {
        let inner = level_obj.borrow();
        let lb = inner.parent_of_kind(UObjectKind::LevelBase);
        let Some(lb) = lb else {
            tracing::debug!("post_cascade: MyLevel has no LevelBase parent");
            return Ok(());
        };
        let Some(lb) = lb.as_any().downcast_ref::<LevelBase>() else {
            tracing::debug!("post_cascade: LevelBase downcast failed");
            return Ok(());
        };
        lb.actors.iter().filter_map(|a| a.clone()).collect()
    };
    tracing::info!(
        "post_cascade: walking {} actors in {}",
        actors.len(),
        secondary_package
    );

    // For each actor, walk the engine-init function bytecode across
    // its class chain. Mirrors SC's `LoadMap` (xbe `0x81860`), which
    // calls these via `ProcessEvent` on every Level.Actors[] entry
    // in order:
    //   1. PreBeginPlay
    //   2. BeginPlay
    //   3. (vtable[0xcc] / [0xd0] — native, no script)
    //   4. PostBeginPlay
    //   5. (vtable[0xd4] — native)
    //   6. SetInitialState
    // Each script can call DynamicLoadObject(class'X', "Pkg.Name") to
    // trigger an asset load; we walk the bytecode, recognize those
    // calls, and forward them through `load_object_by_full_name` so
    // the cursor advances in the same order the engine's. Cycle
    // guard de-dupes the per-class walk so a shared base class only
    // pays its bytecode-walk cost once.
    let init_fns = ["PreBeginPlay", "BeginPlay", "PostBeginPlay", "SetInitialState"];
    let mut visited: HashSet<ClassLeafKey> = HashSet::new();
    for actor in &actors {
        let Some(class_obj) = class_of_actor(actor, runtime) else {
            continue;
        };
        if walk_class_chain::<E, _>(
            &class_obj,
            &class_obj,
            &init_fns,
            &mut visited,
            runtime,
            reader,
        )? == WalkOutcome::Aborted
        {
            tracing::info!("post_cascade: aborted (cursor diverged from engine call sequence)");
            break;
        }
    }
    Ok(())
}

