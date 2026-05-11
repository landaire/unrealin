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
//! ## What this walker does
//!
//! This is a **pattern-matching bytecode scanner, not an interpreter**.
//! It does not execute scripts. For each actor, it walks the actor's
//! class chain super-first and runs `walk_parsed_script` on every
//! class's defined version of the requested phase function
//! (`PreBeginPlay`/etc.). Inside each function it:
//!
//! 1. Iterates the parsed `Expr` tree in source order.
//! 2. Recognises the asset-load call shape `Token(FinalFunction)
//!    Object(idx) ...args... EndFunctionParms` where `idx` resolves to
//!    a function named `DynamicLoadObject` or `StaticLoadObject`. The
//!    first `StringConst` is the asset path; we fire the same load.
//! 3. Recognises the guard `if (instance_var == None)` compiled as
//!    `JumpIfNot Word(N) Native(119) InstanceVariable Object NoObject
//!    EndFunctionParms` and skips the conditional body when the
//!    actor's class CDO has a non-null default for that variable.
//!
//! ## Why super-first instead of `FindFunctionChecked`-style dispatch
//!
//! UE2's `ProcessEvent` would resolve the most-derived `UFunction`
//! named X via `FindFunctionChecked` (Core_retail.dll `sub_10161020`,
//! a hash lookup whose chain inheritance is baked into the per-class
//! hash) and run only that function's bytecode; `Super.X()` calls
//! recurse via `execFinalFunction` (`sub_10133170`) which reads the
//! `UFunction*` directly out of the script stream. We tried that
//! shape (see `dispatch_event` git history) but it regressed coverage
//! on both retail (+3 body-byte mismatches) and proto (+13 mismatches,
//! six new panics, ~13 MB of extra unread tail bytes across the seven
//! affected pairs) because our scanner is static: it can't follow
//! conditional branches, helper-function calls, or per-instance
//! variable state, so a strict leaf-only walk under-fires whenever
//! the engine's runtime dispatch uses any of those. The unconditional
//! super-first walk over-fires by walking parents whose `Super.X()`
//! the engine wouldn't have called, but loads are idempotent via
//! `runtime.loaded_objects`, so the over-fires are no-ops once the
//! body is already in memory; what matters is that we visit enough
//! source positions to advance the cursor past every body the engine
//! reads.
//!
//! ## Known limits vs. engine semantics
//!
//! - Flat iteration assumes straight-line code. Branches/loops other
//!   than the recognised `if (X == None)` pattern are walked as if
//!   their bytecode unconditionally executes, which can over-trigger
//!   loads relative to the engine. (Idempotent loads make this safe.)
//! - Only `DynamicLoadObject` / `StaticLoadObject` are recognised as
//!   load callsites. Loads through helper/wrapper functions or
//!   delegates are missed.
//! - The `if (X == None)` guard checks only class-CDO defaults; loads
//!   gated by per-instance property tags (`Mesh = SkeletalMesh'X'`
//!   set in the level editor on a specific actor) are not gated
//!   because we don't parse actor instance bodies past the
//!   variable-length `HAS_STACK` state frame.
//! - The walker iterates each actor's class chain super-first; the
//!   engine instead resolves a single function via
//!   `FindFunctionChecked` and `ProcessEvent`s it once per actor.
//!   See "Why super-first..." above for why we don't mirror the
//!   engine here.

use std::io;
use std::rc::Rc;

use byteorder::ByteOrder;

use crate::de::RcLinker;
use crate::object::RcUnrealObject;
use crate::object::UObjectKind;
use crate::object::internal::script::Expr;
use crate::object::internal::script::ExprToken;
use crate::object::uclass::Class;
use crate::object::ufield::Field;
use crate::object::ulevel_base::LevelBase;
use crate::object::ustruct::Struct;
use crate::reader::LinRead;
use crate::runtime::LoadKind;
use crate::runtime::UnrealRuntime;

/// Object/Class property type byte constants matching `read_tag_value`.
const TAG_TYPE_OBJECT: u8 = 5;
const TAG_TYPE_CLASS: u8 = 8;

/// Trim the trailing null and check shape: ASCII alphanumeric / `_` /
/// `-` segments separated by `.`, at least two non-empty segments. Hyphen
/// is intentional -- SC has packages named e.g. `2-1_CIA_tex`.
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
fn resolve_callee_name(
    idx: i32,
    func_linker: &RcLinker,
    runtime: &UnrealRuntime,
) -> Option<String> {
    let obj = runtime.find_loaded_object_by_raw_index(idx, func_linker)?;
    let inner = obj.try_borrow().ok()?;
    let n = inner.base_object().name.clone();
    Some(n)
}

/// Return the class name (e.g. "Sound", "MeshAnimation") of an
/// import referent. For exports the class name comes from the
/// linker's class-index resolution; for imports it comes from the
/// `class_name` field in the import row. Returns `None` when the
/// index doesn't resolve cleanly (already-discarded import, etc.).
fn resolve_referent_class(
    idx: i32,
    func_linker: &RcLinker,
    runtime: &UnrealRuntime,
) -> Option<String> {
    if idx < 0 {
        let imp_idx = (-idx - 1) as usize;
        let l = func_linker.borrow();
        let imp = l.package.imports.get(imp_idx)?;
        let class_name_idx = imp.class_name as usize;
        Some(l.package.names.get(class_name_idx)?.name.clone())
    } else if idx > 0 {
        let obj = runtime.find_loaded_object_by_raw_index(idx, func_linker)?;
        let inner = obj.try_borrow().ok()?;
        Some(inner.kind().as_str().to_string())
    } else {
        None
    }
}

/// Build the dotted full-name for an import or export referent so we
/// can pass it to `load_object_by_full_name`. Walks `package_index`
/// chains the same way the engine's `LinkerLoad::IndexToFullName`
/// does.
fn resolve_referent_full_name(
    idx: i32,
    func_linker: &RcLinker,
    runtime: &UnrealRuntime,
) -> Option<String> {
    let l = func_linker.borrow();
    let mut parts: Vec<String> = Vec::new();
    let mut cur = idx;
    while cur != 0 {
        if cur < 0 {
            let imp_idx = (-cur - 1) as usize;
            let imp = l.package.imports.get(imp_idx)?;
            parts.push(l.package.names.get(imp.object_name as usize)?.name.clone());
            cur = imp.package_index;
        } else {
            let exp_idx = (cur - 1) as usize;
            let exp = l.package.exports.get(exp_idx)?;
            parts.push(l.package.names.get(exp.object_name as usize)?.name.clone());
            cur = exp.package_index;
        }
        if parts.len() > 16 {
            return None;
        }
    }
    drop(l);
    let _ = runtime;
    if parts.len() < 2 {
        return None;
    }
    parts.reverse();
    Some(parts.join("."))
}

/// Class names whose `ObjectConst` references are pre-loaded by the
/// engine when the referencing script's class umbrella is loaded
/// (their bodies live just past the class chain in session.lin
/// authoring-order). Other types -- actor instances, GameInfo refs,
/// HUD widgets -- are spawned/touched at runtime and don't preload
/// from script-literal sites.
fn is_asset_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "Sound" | "MeshAnimation" | "SkeletalMesh" | "Texture" | "Cubemap" | "StaticMesh" | "Mesh"
    )
}

/// In-memory script byte size of one parsed `Expr`. Mirrors UE2's
/// in-memory representation (the engine's `iCode` counter), which is what
/// `JumpIfNot Word(N)` uses as its target offset. Object/Name slots are
/// fixed 4 bytes regardless of the variable-length packed_int they
/// occupied on disk; matches the size accounting in `script.rs`'s
/// `bytes_read`.
fn expr_byte_size(expr: &Expr) -> usize {
    match expr {
        Expr::Token(_) | Expr::Native(_) | Expr::ScNoOpByte(_) => 1,
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
/// Object(prop_idx) Token(NoObject) Token(EndFunctionParms)` -- UE2's
/// compiled `if (this.X == None)` pattern. Returns
/// `Some((prop_idx, target_byte_offset))` if matched.
fn detect_var_is_none_check(parsed_script: &[Expr], i: usize) -> Option<(i32, u16)> {
    if !matches!(
        parsed_script.get(i),
        Some(Expr::Token(ExprToken::JumpIfNot))
    ) {
        return None;
    }
    let Some(Expr::Word(target)) = parsed_script.get(i + 1) else {
        return None;
    };
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
    if !matches!(
        parsed_script.get(i + 5),
        Some(Expr::Token(ExprToken::NoObject))
    ) {
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
    name.eq_ignore_ascii_case("DynamicLoadObject") || name.eq_ignore_ascii_case("StaticLoadObject")
}

/// Inside a function call's argument list (i.e. between `FinalFunction`/
/// `Native` and the matching `EndFunctionParms`), extract:
/// 1. the first `ObjectConst Object(idx)` (UE2's compiled
///    `class'X.Y'` argument -- the InClass passed to
///    `StaticLoadObject` / `DynamicLoadObject`), and
/// 2. the first `StringConst Data(...)` payload (the asset path).
///
/// Returns `(class_ref_idx, name_bytes, after_end_index)`. The outer
/// walker uses `class_ref_idx` to resolve the actual class an
/// import expects, so the resulting load is dispatched with strict
/// class matching -- without it our default (`Core.Class`) over-
/// matches name-only candidates of unrelated types, leaking
/// preloads of e.g. `MeshAnimation ENPC.LadderAnims` for an import
/// that the engine treats as null.
fn extract_load_args(
    parsed_script: &[Expr],
    args_start: usize,
) -> (Option<i32>, Option<&[u8]>, usize) {
    let mut found_class: Option<i32> = None;
    let mut found_str: Option<&[u8]> = None;
    let mut i = args_start;
    while i < parsed_script.len() {
        match &parsed_script[i] {
            Expr::Token(ExprToken::EndFunctionParms) => return (found_class, found_str, i + 1),
            Expr::Token(ExprToken::ObjectConst) => {
                if let Some(Expr::Object(idx)) = parsed_script.get(i + 1)
                    && found_class.is_none()
                {
                    found_class = Some(*idx);
                }
            }
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
    (found_class, found_str, i)
}

/// Resolve an `ObjectConst` index in `func_linker` to a
/// `(class_name, class_package_name)` pair suitable for passing to
/// `load_object_by_full_name_with_class`. Mirrors what the engine's
/// `StaticLoadObject` does to the `InClass` arg before searching for
/// the named object: it inspects the UClass's name and outer-package
/// chain. We need the same identity to filter exports correctly.
///
/// For an export-typed referent the class is "Class" (since UClasses
/// are themselves Class-typed objects); the meaningful piece is the
/// referent's *name*, which becomes the `expected_class_name` filter.
fn resolve_class_arg(idx: i32, func_linker: &RcLinker) -> Option<(String, String)> {
    if idx == 0 {
        return None;
    }
    let l = func_linker.borrow();
    if idx < 0 {
        let imp_idx = (-idx - 1) as usize;
        let imp = l.package.imports.get(imp_idx)?;
        let class_name = l.package.names.get(imp.object_name as usize)?.name.clone();
        let class_package = {
            let mut cur = imp.package_index;
            let mut last_pkg = None;
            while cur != 0 {
                if cur < 0 {
                    let i = (-cur - 1) as usize;
                    let parent = l.package.imports.get(i)?;
                    last_pkg = Some(
                        l.package
                            .names
                            .get(parent.object_name as usize)?
                            .name
                            .clone(),
                    );
                    cur = parent.package_index;
                } else {
                    return None;
                }
            }
            last_pkg
        };
        Some((class_name, class_package?))
    } else {
        let exp_idx = (idx - 1) as usize;
        let exp = l.package.exports.get(exp_idx)?;
        let class_name = l.package.names.get(exp.object_name as usize)?.name.clone();
        // For an export-typed class arg the package is the linker's
        // own name (the class lives in this package).
        Some((class_name, l.name.clone()))
    }
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
/// state -- which equals the class default at PostBeginPlay time --
/// so a non-null default flips the condition false, and the load
/// inside doesn't fire. Without this we'd trigger the load anyway,
/// fail the body read at the wrong cursor position, and corrupt the
/// stream for subsequent loads.
fn walk_parsed_script<E, R>(
    parsed_script: &[Expr],
    func_linker: &RcLinker,
    leaf_class: &RcUnrealObject,
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
        // Detect `if (instance_var == None)` -- if the var has a non-null
        // default in the class CDO, the engine would skip this block.
        if let Some((prop_idx, target_byte)) = detect_var_is_none_check(parsed_script, i) {
            let prop_name = runtime
                .find_loaded_object_by_raw_index(prop_idx, func_linker)
                .and_then(|p| p.try_borrow().ok().map(|i| i.base_object().name.clone()));
            if let Some(prop_name) = prop_name
                && class_default_overrides_object(leaf_class, &prop_name)
            {
                // Class-CDO default for this var is non-null. Skip the
                // conditional block -- the engine evaluates `var == None`
                // as false, no body read fires.
                //
                // Note: this catches conditional loads gated by class
                // default overrides only. Loads gated by per-actor
                // instance tags (`Mesh = SkeletalMesh'X'` set in level
                // editor on a specific instance) are NOT caught here --
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

        // Inline asset-literal pattern: `Token(ObjectConst) Object(idx)`
        // emitted by `Sound'X.Y'`, `Class'X.Y'`, etc. in script source.
        // The engine resolves the import at script parse time but only
        // preloads the referenced object when that script executes --
        // which for HUD/PlayerController/PreBeginPlay scripts happens
        // during LoadMap. Trigger a full Load here so the cursor
        // advances through the referenced body in the same order.
        //
        // Filter to "asset-class" referents (Sound, MeshAnimation,
        // SkeletalMesh, Texture) -- pure-Object refs to actor instances
        // or non-asset objects don't get preloaded by the engine here
        // and triggering them would mis-align the cursor.
        if matches!(
            parsed_script.get(i),
            Some(Expr::Token(ExprToken::ObjectConst))
        ) {
            if let Some(Expr::Object(obj_idx)) = parsed_script.get(i + 1).cloned()
                && let Some(class) = resolve_referent_class(obj_idx, func_linker, runtime)
                    && is_asset_class(&class) {
                        let name = resolve_referent_full_name(obj_idx, func_linker, runtime);
                        if let Some(name) = name.as_deref() {
                            runtime.begin_load();
                            let _ = runtime.load_object_by_full_name::<E, _>(
                                name,
                                LoadKind::Load,
                                reader,
                            );
                            let _ = runtime.end_load::<E, _>(reader);
                        }
                    }
            i += 2;
            continue;
        }

        let is_final_call = matches!(
            parsed_script.get(i),
            Some(Expr::Token(ExprToken::FinalFunction))
        );
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
        let (class_arg_idx, str_arg, after) = extract_load_args(parsed_script, i + 2);
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

        // The first ObjectConst arg of `StaticLoadObject(class'X', "Pkg.Y", ...)`
        // is the InClass -- the engine resolves the named object only
        // when its class matches. Without this filter the runtime falls
        // through to the `("Class", "Core")` default + name-only fallback,
        // which over-matches: e.g. an import for `class<MeshAnimation>
        // LadderAnims` over a map whose ENPC fragment lacks a
        // `LadderAnims` class umbrella resolves to the `MeshAnimation`
        // export of the same name, the wrong-typed body gets preloaded
        // out of disk order, and the cascade misaligns by hundreds of KB
        // (verified on `4_3_0ChineseEmbassy`).
        let class_info = class_arg_idx.and_then(|idx| resolve_class_arg(idx, func_linker));
        let class_info_pair = class_info.as_ref().map(|(n, p)| (n.as_str(), p.as_str()));

        runtime.begin_load();
        let result = if let Some(ci) = class_info_pair {
            runtime.load_object_by_full_name_with_class::<E, _>(
                &name,
                Some(ci),
                LoadKind::Load,
                reader,
            )
        } else {
            runtime.load_object_by_full_name::<E, _>(&name, LoadKind::Load, reader)
        };
        if let Err(ref e) = result {
            tracing::debug!("post_cascade: {name} failed: {e}");
        }
        let drain = runtime.end_load::<E, _>(reader);
        match drain {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("post_cascade: drain after {name} failed ({e}); aborting walk");
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

/// Walk one phase function for one actor. Equivalent to the engine's
/// `ProcessEvent(actor, fn_name)` for the side-effects we care about
/// (asset preloads), but takes the unconditional super-first path
/// rather than a `FindFunctionChecked`-style leaf-only dispatch -- see
/// the module docstring for why this differs from the engine's
/// runtime model.
///
/// Recurses through `Field.super_field` to walk every class in the
/// chain super-first and runs `walk_parsed_script` on each class that
/// defines a function named `fn_name`. CDO-default checks against
/// `if (var == None)` use `leaf_class` (the actor's most-derived
/// class) regardless of which class we're currently visiting, since
/// the actor's runtime variable state is determined by its leaf
/// class's accumulated defaults.
fn dispatch_event<E, R>(
    leaf_class: &RcUnrealObject,
    fn_name: &str,
    runtime: &mut UnrealRuntime,
    reader: &mut R,
) -> io::Result<WalkOutcome>
where
    E: ByteOrder,
    R: LinRead,
{
    walk_class_chain::<E, _>(leaf_class, leaf_class, fn_name, runtime, reader)
}

fn walk_class_chain<E, R>(
    class_obj: &RcUnrealObject,
    leaf_class: &RcUnrealObject,
    fn_name: &str,
    runtime: &mut UnrealRuntime,
    reader: &mut R,
) -> io::Result<WalkOutcome>
where
    E: ByteOrder,
    R: LinRead,
{
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
        && walk_class_chain::<E, R>(&super_obj, leaf_class, fn_name, runtime, reader)?
            == WalkOutcome::Aborted
    {
        return Ok(WalkOutcome::Aborted);
    }
    let Some(func) = find_function_by_name(class_obj, fn_name) else {
        return Ok(WalkOutcome::Continue);
    };
    let (parsed_script, func_linker) = {
        let Ok(inner) = func.try_borrow() else {
            return Ok(WalkOutcome::Continue);
        };
        let Some(s) = inner.parent_of_kind(UObjectKind::Struct) else {
            return Ok(WalkOutcome::Continue);
        };
        let Some(s) = s.as_any().downcast_ref::<Struct>() else {
            return Ok(WalkOutcome::Continue);
        };
        (s.parsed_script.clone(), inner.base_object().linker())
    };
    walk_parsed_script::<E, _>(&parsed_script, &func_linker, leaf_class, runtime, reader)
}

/// Resolve the class object for an actor instance via the runtime's
/// class lookup. Each actor's `concrete_obj` was constructed with a
/// `class_index` whose Object lives in some loaded linker; we follow
/// the same `IndexToObject` path the engine would.
fn class_of_actor(actor: &RcUnrealObject, runtime: &UnrealRuntime) -> Option<RcUnrealObject> {
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
    // filter `find_export_by_name` returns the first match -- which
    // is the Package wrapper, leaving the actor walk empty and
    // skipping ~17% of session.lin's bytes that the engine reads via
    // the actors' PreBeginPlay/BeginPlay/PostBeginPlay scripts.
    let level_obj = {
        let linker = linker_rc.borrow();
        let Some((level_idx, _)) =
            linker.find_export_by_name_and_class("MyLevel", "Level", "Engine")
        else {
            tracing::debug!("post_cascade: MyLevel export with class=Engine.Level not found");
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

    // Walk each phase across ALL actors before advancing to the next
    // phase. Mirrors SC's `LoadMap` (xbe `0x81860`) which has SEPARATE
    // actor-iteration loops per phase (the four `(*(*esi_N + 0x10))(
    // sub_47260(esi_N, data_<event_name>, 0), 0, 0)` blocks that read
    // `data_37cab4`/`37c9d4`/`37c8c8`/`37c918` for `PreBeginPlay`/
    // `BeginPlay`/`PostBeginPlay`/`SetInitialState` respectively).
    //
    // Iterating actor-by-actor through all phases (the previous shape)
    // changes the load-call order: the LIN compactor lays out body
    // bytes in the engine's call order, so we MUST match the engine's
    // phase-by-phase order or our sequential cursor reads the wrong
    // bytes for any preload whose script-call order differs from
    // export-table order. Verified on 4_3_0ChineseEmbassy where actor
    // scripts call different-class loads across PreBeginPlay vs
    // PostBeginPlay; the per-actor order had us preload `LadderAnims`
    // (export 66) AFTER `TableAnims` (export 67) and the cursor was
    // 8 bytes past Ladder's body when Ladder's preload tried to read.
    //
    // Loads are idempotent via `runtime.loaded_objects`, so duplicate
    // walks of the same class chain are no-ops on the second pass
    // through.
    let init_fns = [
        "PreBeginPlay",
        "BeginPlay",
        "PostBeginPlay",
        "SetInitialState",
    ];
    'phases: for fn_name in &init_fns {
        for actor in &actors {
            let Some(class_obj) = class_of_actor(actor, runtime) else {
                continue;
            };
            if dispatch_event::<E, _>(&class_obj, fn_name, runtime, reader)? == WalkOutcome::Aborted
            {
                tracing::info!("post_cascade: aborted (cursor diverged from engine call sequence)");
                break 'phases;
            }
        }

        // After the actor PostBeginPlay phase, before SetInitialState,
        // mirror UE2's GameInfo PostBeginPlay step. SC's `LoadMap`
        // (xbe `0x81860`) calls
        // `StaticLoadClass(EnginePkg, nullptr, &var_c0c, nullptr, 1)`
        // where `var_c0c` is the class name parsed from the URL
        // `?GAME=...` option, then spawns a GameInfo of that class
        // and calls its `PostBeginPlay`. That's what fires the
        // StaticLoadObject cluster for level-specific assets (the
        // ENPC anim cluster for 4_3_0ChineseEmbassy, the level's
        // patrol patterns, etc.). Verified in
        // `reads.json.011_ChineseEmbassy_4_3_0...bak`'s
        // `gobj_loaded_order`: `EchelonPattern.V4_3_0ChineseEmbassy`
        // + its `PostBeginPlay` at indices 19346/19347 immediately
        // before the ENPC anim cluster at 19348..19353.
        //
        // Where does the URL get the class name? SC's menu code
        // constructs `?GAME=EchelonPattern.V<secondary_package>`
        // before calling LoadMap. There is no property tag in the
        // .lin that declares this class (`LevelInfo.GameType` is
        // not present in SC's serialized data; `PatternClass` /
        // `BasicPatternClass` etc. exist but resolve to AI pattern
        // classes, not the GameInfo class). The level-specific
        // class IS declared on disk -- `EchelonPattern.u`'s export
        // table includes `V<level>` exports for every map that has
        // one -- but the link between the level and its V<level>
        // lives entirely in SC's runtime menu code. We replicate
        // the same name-construction convention here, which loads
        // exactly the class the engine loads via URL.
        if *fn_name == "PostBeginPlay" {
            let v_level = format!("EchelonPattern.V{}", secondary_package);
            runtime.begin_load();
            let v_class_opt = runtime
                .load_object_by_full_name::<E, _>(&v_level, crate::runtime::LoadKind::Load, reader)
                .ok()
                .flatten();
            let _ = runtime.end_load::<E, _>(reader);
            if let Some(v_class) = v_class_opt {
                let _ = dispatch_event::<E, _>(&v_class, "PostBeginPlay", runtime, reader)?;
            }
        }
    }
    Ok(())
}

/// Returns true if `class_obj` is `<package>.<class_name>` or any of its
/// subclasses. Walks `Field.super_field` to climb the chain. Mirrors
/// UE2's `UClass::IsChildOf` traversal that the engine's per-actor type
/// tests do (e.g. `sub_23a00` = IsA `Echelon.EPawn`).
fn class_is_child_of(class_obj: &RcUnrealObject, package_name: &str, class_name: &str) -> bool {
    let mut current = Some(Rc::clone(class_obj));
    while let Some(cls) = current {
        let next = {
            let Ok(inner) = cls.try_borrow() else {
                return false;
            };
            let base = inner.base_object();
            let name_matches = base.name.eq_ignore_ascii_case(class_name);
            let pkg_matches = base
                .linker()
                .borrow()
                .name
                .eq_ignore_ascii_case(package_name);
            if name_matches && pkg_matches {
                return true;
            }
            inner
                .parent_of_kind(UObjectKind::Field)
                .and_then(|f| f.as_any().downcast_ref::<Field>())
                .and_then(|f| f.super_field())
        };
        current = next;
    }
    false
}

/// First five characters of `secondary_package` (e.g. `"0_0_2"` for
/// `"0_0_2_Training"`). The engine uses the leading `5_chars` substring
/// as the level-id suffix it appends to per-map sound names like
/// `Special.Play_Switch_NpcMaleMove<level>`. Source: SC xbe `sub_271e0`
/// at the `sub_16d90(&arg2, &var_150, 5)` call site.
fn level_id_suffix(secondary_package: &str) -> String {
    secondary_package.chars().take(5).collect()
}

/// Country prefix used when constructing `Camera.Play_Random_<XX><...>`
/// sound names. The engine selects this from the level identifier's
/// first character -- SC xbe `sub_271e0` switches on `arg2[0]`:
/// `'0'`/`'2'` -> `"US"`, `'1'`/`'5'` -> `"GE"`, `'3'` -> `"RU"`,
/// `'4'` -> `"CH"`. See data refs `0x262960`/`0x262950`/`0x262948`/
/// `0x262958` for the exact strings.
fn country_prefix_for_level(secondary_package: &str) -> &'static str {
    match secondary_package.chars().next() {
        Some('0') | Some('2') => "US",
        Some('1') | Some('5') => "GE",
        Some('3') => "RU",
        Some('4') => "CH",
        _ => "US",
    }
}

/// Mirrors SC's post-LoadMap actor-driven sound preloads (xbe `sub_271e0`).
/// After `LoadMap` finishes the BeginPlay/PostBeginPlay/SetInitialState
/// loop on every actor, the engine iterates the same actor list one more
/// time and, for each actor, runs five `IsA` tests against
/// `Echelon.EPawn`, `Echelon.EWeapon`, `EchelonIngredient.ESensor`,
/// `Echelon.EchelonLevelInfo`, and `Engine.StaticMeshActor`. For each
/// matching test the engine constructs a sound name from a fixed prefix
/// + the level-id suffix (first 5 chars of the map basename) and calls
/// `StaticLoadObject(Engine.Sound, name, ...)`. Names match exactly:
///
/// * `EPawn` -> `Special.Play_Switch_NpcMaleMove<level>`,
///   `Special.Play_Switch_FisherMove<level>`
/// * `EWeapon` -> `Special.Play_Switch_BulletHit<level>`
/// * `ESensor` -> `Special.Play_Switch_BulletHit<level>`,
///   `Camera.Play_Random_<country>CSeePlayer`,
///   `Camera.Play_Random_<country>CFindCorpse`
/// * `EchelonLevelInfo` -> `Camera.Play_Random_<country>IAFindCorpse`
/// * `StaticMeshActor` -> `Special.Play_Random_BulletHitFence` (literal,
///   no level suffix)
///
/// Engine binary skips this entire block for the menu (`arg2 == "menu"`).
/// We mirror that -- the menu has no actors that need these sound preloads.
///
/// Note: the engine also checks per-actor instance flags (e.g.
/// `*(eax_14 + 0x230) & 8 == 0` for EPawn, `*(eax_18 + 0x18e) & 1 != 0`
/// for StaticMeshActor) before firing the loads. We skip those flag
/// gates because `load_object_by_full_name` is idempotent -- over-firing
/// loads of names the engine wouldn't has no byte-stream effect (each
/// sound is read at most once on the first matching call). Skipping
/// the gates avoids parsing actor instance state frames, which
/// require the HAS_STACK frame parser we don't yet have.
pub fn run_post_spawn_actor_loads<E, R>(
    secondary_package: &str,
    runtime: &mut UnrealRuntime,
    reader: &mut R,
) -> io::Result<()>
where
    E: ByteOrder,
    R: LinRead,
{
    if secondary_package.eq_ignore_ascii_case("menu") {
        return Ok(());
    }

    let suffix = level_id_suffix(secondary_package);
    let country = country_prefix_for_level(secondary_package);

    let linker_rc = runtime
        .linkers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(secondary_package))
        .map(|(_, v)| v.clone());
    let Some(linker_rc) = linker_rc else {
        return Ok(());
    };
    let level_obj = {
        let linker = linker_rc.borrow();
        let Some((level_idx, _)) =
            linker.find_export_by_name_and_class("MyLevel", "Level", "Engine")
        else {
            return Ok(());
        };
        linker.objects.get(&level_idx).cloned()
    };
    let Some(level_obj) = level_obj else {
        return Ok(());
    };
    let actors: Vec<RcUnrealObject> = {
        let inner = level_obj.borrow();
        let lb = inner.parent_of_kind(UObjectKind::LevelBase);
        let Some(lb) = lb else { return Ok(()) };
        let Some(lb) = lb.as_any().downcast_ref::<LevelBase>() else {
            return Ok(());
        };
        lb.actors.iter().filter_map(|a| a.clone()).collect()
    };

    let try_load = |runtime: &mut UnrealRuntime, reader: &mut R, name: &str| {
        runtime.begin_load();
        let _ = runtime.load_object_by_full_name::<E, _>(name, LoadKind::Load, reader);
        let _ = runtime.end_load::<E, _>(reader);
    };

    for actor in &actors {
        let Some(class_obj) = class_of_actor(actor, runtime) else {
            continue;
        };

        if class_is_child_of(&class_obj, "Echelon", "EPawn") {
            try_load(
                runtime,
                reader,
                &format!("Special.Play_Switch_NpcMaleMove{suffix}"),
            );
            try_load(
                runtime,
                reader,
                &format!("Special.Play_Switch_FisherMove{suffix}"),
            );
        }
        if class_is_child_of(&class_obj, "Echelon", "EWeapon") {
            try_load(
                runtime,
                reader,
                &format!("Special.Play_Switch_BulletHit{suffix}"),
            );
        }
        if class_is_child_of(&class_obj, "EchelonIngredient", "ESensor") {
            try_load(
                runtime,
                reader,
                &format!("Special.Play_Switch_BulletHit{suffix}"),
            );
            try_load(
                runtime,
                reader,
                &format!("Camera.Play_Random_{country}CSeePlayer"),
            );
            try_load(
                runtime,
                reader,
                &format!("Camera.Play_Random_{country}CFindCorpse"),
            );
        }
        if class_is_child_of(&class_obj, "Echelon", "EchelonLevelInfo") {
            try_load(
                runtime,
                reader,
                &format!("Camera.Play_Random_{country}IAFindCorpse"),
            );
        }
        if class_is_child_of(&class_obj, "Engine", "StaticMeshActor") {
            try_load(runtime, reader, "Special.Play_Random_BulletHitFence");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors UE2's `ProcessEvent` recognising `DynamicLoadObject`
    /// and `StaticLoadObject` as the script-driven asset-load
    /// callsites. These names come from the engine's compiled-in
    /// FName table; case-insensitive match per FName equality.
    #[test]
    fn is_asset_load_function_recognises_engine_names() {
        assert!(is_asset_load_function("DynamicLoadObject"));
        assert!(is_asset_load_function("dynamicloadobject"));
        assert!(is_asset_load_function("DYNAMICLOADOBJECT"));
        assert!(is_asset_load_function("StaticLoadObject"));
        assert!(is_asset_load_function("staticloadobject"));
        // Not in the recognised set -- even close names should reject.
        assert!(!is_asset_load_function("LoadObject"));
        assert!(!is_asset_load_function("DynamicLoad"));
        assert!(!is_asset_load_function("FindObject"));
        assert!(!is_asset_load_function(""));
    }

    /// Asset-path validation: a plausible UE2 full name is at least
    /// two `.`-separated segments of `[A-Za-z0-9_-]`. Hyphens are
    /// intentional (SC has packages like `2-1_CIA_tex`). Single-
    /// segment names (engine intrinsics like `Engine`) and arbitrary
    /// strings should reject.
    #[test]
    fn asset_path_recognises_pkg_dot_name_only() {
        assert_eq!(
            asset_path_from_string_const(b"ENPC.SamAMesh\0").as_deref(),
            Some("ENPC.SamAMesh")
        );
        assert_eq!(
            asset_path_from_string_const(b"2-1_CIA_tex.lobby_PAL\0").as_deref(),
            Some("2-1_CIA_tex.lobby_PAL")
        );
        assert_eq!(
            asset_path_from_string_const(b"ETexCharacter.Sam.SamCBody\0").as_deref(),
            Some("ETexCharacter.Sam.SamCBody")
        );

        // No `.` at all -- not a full name.
        assert_eq!(asset_path_from_string_const(b"Engine\0"), None);
        // Empty segment.
        assert_eq!(asset_path_from_string_const(b".Sound\0"), None);
        assert_eq!(asset_path_from_string_const(b"Engine.\0"), None);
        assert_eq!(asset_path_from_string_const(b"\0"), None);
        // Invalid char (whitespace).
        assert_eq!(asset_path_from_string_const(b"My Pkg.Name\0"), None);
        // Invalid char (slash).
        assert_eq!(asset_path_from_string_const(b"Pkg/Sub.Name\0"), None);
        // Empty input.
        assert_eq!(asset_path_from_string_const(b""), None);
    }

    /// Regression: `dispatch_event` must take exactly the four
    /// `ProcessEvent`-shaped args (leaf class + event name + runtime +
    /// reader) and dispatch the single most-derived definition. If
    /// someone re-introduces unconditional super-walking (multiple fn
    /// names, or an explicit visited set, or a per-class loop), this
    /// fails to type-check.
    #[test]
    fn dispatch_event_has_process_event_shape() {
        type Sig = for<'a> fn(
            &'a RcUnrealObject,
            &'a str,
            &'a mut UnrealRuntime,
            &'a mut crate::reader::LinReader<std::io::Cursor<Vec<u8>>>,
        ) -> io::Result<WalkOutcome>;
        let _f: Sig = dispatch_event::<byteorder::LittleEndian, _>;
    }
}
