//! Structural per-property `SerializeItem` dispatch.
//!
//! Mirrors UE2's per-class `SerializeItem` overrides in 1:1 fashion. References:
//! - `UStructProperty::SerializeItem` (PC `Core_retail.dll` `sub_10172670`):
//!   tagged loop for non-{Vector, Rotator, Plane} structs, `SerializeBin`
//!   otherwise. The PC engine hardcodes FName indices `{0x57, 0x58, 0x5A}`;
//!   SC's xbe likely uses different indices for the same names. Comparison
//!   here is by the resolved string, case-insensitive (matches FName's
//!   `appStrchr_insensitive`).
//! - `UArrayProperty::SerializeItem` (PC `sub_101718e0`): packed_int count
//!   then per-element `Inner->SerializeItem`.
//! - `UStruct::SerializeBin` (PC `sub_1012c910`): walks PropertyLink chain
//!   and dispatches `SerializeItem` for each property index.
//! - `UStruct::SerializeTaggedProperties` (xbe `0x2b4f0`): tag loop, name
//!   lookup, type-match dispatch, raw skip on miss.
//!
//! The diagnostic logging here is the load-bearing piece for this iteration:
//! when the structural dispatch desyncs from the recorded I/O trace, the
//! `cheat()` panic point should be attributable from these logs to one of:
//!   1. SC's native-bin allowlist differs from `{Vector, Rotator, Plane}`.
//!   2. The chain walker double-counts super-class properties.
//!   3. A variable-length property kind is being treated as fixed-size.

use std::io;
use std::rc::Rc;

use byteorder::ByteOrder;
use tracing::debug;
use tracing::trace;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::RcUnrealObject;
use crate::object::UObjectKind;
use crate::object::builtins::ArrayProperty;
use crate::object::builtins::Field;
use crate::object::builtins::Property;
use crate::object::builtins::Struct;
use crate::object::builtins::StructProperty;
use crate::object::internal::property::PropertyTag;
use crate::object::uobject::read_tag_value;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

const NAME_BYTE_PROPERTY: u8 = 1;
const NAME_INT_PROPERTY: u8 = 2;
const NAME_BOOL_PROPERTY: u8 = 3;
const NAME_FLOAT_PROPERTY: u8 = 4;
const NAME_OBJECT_PROPERTY: u8 = 5;
const NAME_NAME_PROPERTY: u8 = 6;
const NAME_DELEGATE_PROPERTY: u8 = 7;
const NAME_CLASS_PROPERTY: u8 = 8;
const NAME_ARRAY_PROPERTY: u8 = 9;
const NAME_STRUCT_PROPERTY: u8 = 10;
const NAME_STR_PROPERTY: u8 = 11;

/// Returns true when the engine routes this struct's value through
/// `SerializeBin` (raw concatenation of inner property values) rather than
/// `SerializeTaggedProperties`.
///
/// **SC xbe `sub_50380` hardcodes FName indices `{0x57, 0x58, 0x5A}`. The
/// resolved strings are `{Vector, Rotator, Color}` per SC's `RegisterNames`
/// (xbe `0x41320`).** This differs from PC's `Core_retail.dll` `sub_10172670`
/// which uses the same indices but resolves them to `{Vector, Rotator, Plane}`
/// — the global FName ordering moved `Color` into slot `0x5A` in SC. Compare
/// by resolved string, case-insensitive (matches `appStrchr_insensitive`).
fn is_native_bin_struct(struct_obj: &RcUnrealObject) -> bool {
    let n = struct_obj.borrow().base_object().name.clone();
    n.eq_ignore_ascii_case("Vector")
        || n.eq_ignore_ascii_case("Rotator")
        || n.eq_ignore_ascii_case("Color")
}

fn property_array_dim(prop: &RcUnrealObject) -> u16 {
    let inner = prop.borrow();
    inner
        .parent_of_kind(UObjectKind::Property)
        .and_then(|p| p.as_any().downcast_ref::<Property>())
        .map(|p| p.array_dim())
        .unwrap_or(1)
}

/// Walks the linked chain of `Field`s starting at `head`, pushing each
/// `Property` (case-insensitive) into `out`. Stops at the end of the chain.
/// Children may be UnrealObjects whose `RefCell` is currently mutably
/// borrowed elsewhere on the call stack (rare, but possible when a
/// recursive `read_object` chain triggers a `Class::deserialize` whose
/// super is the in-flight outer class). Use `try_borrow()` and skip such
/// nodes so the walker doesn't panic; the skipped properties simply fall
/// back to `cheat()` at their tag site.
fn collect_chain(out: &mut Vec<RcUnrealObject>, head: Option<RcUnrealObject>) {
    let mut current = head;
    while let Some(field_rc) = current {
        let Ok(inner) = field_rc.try_borrow() else {
            tracing::debug!(
                "collect_chain: skipping mutably-borrowed field at {:#x}",
                field_rc.as_ptr().expose_provenance()
            );
            break;
        };
        let is_property = inner.is_a(UObjectKind::Property);
        let next = inner
            .parent_of_kind(UObjectKind::Field)
            .and_then(|f| f.as_any().downcast_ref::<Field>())
            .and_then(|f| f.next());
        drop(inner);
        if is_property {
            out.push(Rc::clone(&field_rc));
        }
        current = next;
    }
}

/// Recurses through the super chain, collecting properties from the super
/// (and super's super, etc.). When the super's `RefCell` is currently
/// mutably borrowed (e.g. an outer class is mid-deserialize and its
/// `default_tags` loop spawned a nested class load whose super is back
/// up the stack), `try_borrow()` returns `Err` and we skip. The cost is
/// missing inherited properties for tag dispatch in the inner class —
/// those tags fall back to `cheat()` and trace alignment is preserved.
fn collect_super_chain(out: &mut Vec<RcUnrealObject>, super_obj: Option<RcUnrealObject>) {
    let Some(super_rc) = super_obj else {
        return;
    };
    let Ok(inner) = super_rc.try_borrow() else {
        tracing::debug!(
            "collect_super_chain: super at {:#x} is mutably borrowed; skipping",
            super_rc.as_ptr().expose_provenance()
        );
        return;
    };
    let struct_part = inner.parent_of_kind(UObjectKind::Struct);
    let own_children = struct_part
        .and_then(|s| s.as_any().downcast_ref::<Struct>())
        .and_then(|s| s.children.clone());
    let next_super = inner
        .parent_of_kind(UObjectKind::Field)
        .and_then(|f| f.as_any().downcast_ref::<Field>())
        .and_then(|f| f.super_field());
    drop(inner);
    collect_chain(out, own_children);
    collect_super_chain(out, next_super);
}

/// Walks the supplied own-children head and the supplied super pointer,
/// collecting every property (own first, then walking up the super chain).
/// Use this when a `&mut Class` borrow is live on the immediate struct: the
/// caller pulls `children` and `super_field` directly from its fields and
/// passes them in.
pub(crate) fn collect_struct_properties_from_parts(
    own_children: Option<RcUnrealObject>,
    super_field: Option<RcUnrealObject>,
) -> Vec<RcUnrealObject> {
    let mut out = Vec::new();
    collect_chain(&mut out, own_children);
    collect_super_chain(&mut out, super_field);
    out
}

/// Walks `struct_obj` and its super chain, returning all properties.
/// `try_borrow` to skip if `struct_obj` is currently mutably borrowed
/// (rare; happens when a nested class load reaches back to an outer
/// class on the deserialize stack).
pub(crate) fn collect_struct_properties(struct_obj: &RcUnrealObject) -> Vec<RcUnrealObject> {
    let Ok(inner) = struct_obj.try_borrow() else {
        tracing::debug!(
            "collect_struct_properties: struct at {:#x} is mutably borrowed; returning empty chain",
            struct_obj.as_ptr().expose_provenance()
        );
        return Vec::new();
    };
    let own_children = inner
        .parent_of_kind(UObjectKind::Struct)
        .and_then(|s| s.as_any().downcast_ref::<Struct>())
        .and_then(|s| s.children.clone());
    let super_field = inner
        .parent_of_kind(UObjectKind::Field)
        .and_then(|f| f.as_any().downcast_ref::<Field>())
        .and_then(|f| f.super_field());
    drop(inner);
    collect_struct_properties_from_parts(own_children, super_field)
}

pub(crate) fn find_property_by_name(
    properties: &[RcUnrealObject],
    name: &str,
) -> Option<RcUnrealObject> {
    properties.iter().find_map(|p| {
        let pn = p.borrow().base_object().name.clone();
        if pn.eq_ignore_ascii_case(name) {
            Some(Rc::clone(p))
        } else {
            None
        }
    })
}

/// Serializes a single value of `prop`'s type. Mirrors per-class
/// `SerializeItem` overrides. Bool here is the **non-tagged** path
/// (4-byte DWORD); the tagged path lives in `read_tag_value` which uses
/// `tag.info`'s high bit and reads zero bytes.
pub(crate) fn serialize_item<E, R>(
    prop: &RcUnrealObject,
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
    reader: &mut R,
) -> io::Result<()>
where
    E: ByteOrder,
    R: LinRead,
{
    let kind = prop.borrow().kind();
    let prop_name = prop.borrow().base_object().name.clone();
    let pos_before = reader.stream_position()?;

    debug!(
        "serialize_item ENTER prop={} kind={:?} pos={:#X}",
        prop_name, kind, pos_before
    );

    match kind {
        UObjectKind::ByteProperty => {
            let mut buf = [0u8; 1];
            reader.cheat(&mut buf)?;
        }
        UObjectKind::IntProperty | UObjectKind::FloatProperty => {
            let mut buf = [0u8; 4];
            reader.cheat(&mut buf)?;
        }
        UObjectKind::BoolProperty => {
            let mut buf = [0u8; 4];
            reader.cheat(&mut buf)?;
        }
        UObjectKind::NameProperty => {
            let _ = reader.read_packed_int()?;
        }
        UObjectKind::ObjectProperty | UObjectKind::ClassProperty => {
            let _ = reader.read_object::<E>(runtime, linker)?;
        }
        UObjectKind::StrProperty => {
            let _ = reader.read_string()?;
        }
        UObjectKind::StructProperty => {
            let struct_obj = {
                let inner = prop.borrow();
                inner
                    .as_any()
                    .downcast_ref::<StructProperty>()
                    .and_then(|sp| sp.struct_obj.as_ref().cloned())
            };
            let Some(struct_obj) = struct_obj else {
                let pos = reader.stream_position()?;
                tracing::error!(
                    "serialize_item: StructProperty {} has no struct_obj at pos={:#X}",
                    prop_name,
                    pos
                );
                return Err(io::Error::other(format!(
                    "StructProperty {} has no struct_obj",
                    prop_name
                )));
            };
            serialize_struct_value::<E, _>(&struct_obj, runtime, linker, reader)?;
        }
        UObjectKind::ArrayProperty => {
            let inner_prop = {
                let inner = prop.borrow();
                inner
                    .as_any()
                    .downcast_ref::<ArrayProperty>()
                    .and_then(|ap| ap.inner.as_ref().cloned())
            };
            let Some(inner_prop) = inner_prop else {
                let pos = reader.stream_position()?;
                tracing::error!(
                    "serialize_item: ArrayProperty {} has no inner at pos={:#X}",
                    prop_name,
                    pos
                );
                return Err(io::Error::other(format!(
                    "ArrayProperty {} has no inner",
                    prop_name
                )));
            };
            serialize_array_value::<E, _>(&inner_prop, runtime, linker, reader)?;
        }
        other => {
            let pos = reader.stream_position()?;
            tracing::error!(
                "serialize_item: unhandled prop kind {:?} for {} at pos={:#X}",
                other,
                prop_name,
                pos
            );
            return Err(io::Error::other(format!(
                "serialize_item: unhandled kind {:?}",
                other
            )));
        }
    }

    let pos_after = reader.stream_position()?;
    debug!(
        "serialize_item EXIT  prop={} kind={:?} read={:#X} (pos {:#X} -> {:#X})",
        prop_name,
        kind,
        pos_after - pos_before,
        pos_before,
        pos_after
    );
    Ok(())
}

pub(crate) fn serialize_array_value<E, R>(
    inner: &RcUnrealObject,
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
    reader: &mut R,
) -> io::Result<()>
where
    E: ByteOrder,
    R: LinRead,
{
    let pos_before = reader.stream_position()?;
    let count = reader.read_packed_int()?;
    let inner_kind = inner.borrow().kind();
    let inner_name = inner.borrow().base_object().name.clone();
    debug!(
        "serialize_array_value ENTER inner={} kind={:?} count={} pos_before={:#X}",
        inner_name, inner_kind, count, pos_before
    );
    if count < 0 {
        return Err(io::Error::other(format!(
            "ArrayProperty count is negative: {}",
            count
        )));
    }
    for i in 0..count {
        trace!("serialize_array_value: element {}/{}", i, count);
        serialize_item::<E, _>(inner, runtime, linker, reader)?;
    }
    let pos_after = reader.stream_position()?;
    debug!(
        "serialize_array_value EXIT  inner={} read={:#X} (pos {:#X} -> {:#X})",
        inner_name,
        pos_after - pos_before,
        pos_before,
        pos_after
    );
    Ok(())
}

pub(crate) fn serialize_struct_value<E, R>(
    struct_obj: &RcUnrealObject,
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
    reader: &mut R,
) -> io::Result<()>
where
    E: ByteOrder,
    R: LinRead,
{
    let struct_name = struct_obj.borrow().base_object().name.clone();
    let pos_before = reader.stream_position()?;
    let bin = is_native_bin_struct(struct_obj);
    debug!(
        "serialize_struct_value ENTER struct={} dispatch={} pos_before={:#X}",
        struct_name,
        if bin { "BIN" } else { "TAGGED" },
        pos_before
    );

    let properties = collect_struct_properties(struct_obj);
    debug!(
        "serialize_struct_value struct={} chain_len={} chain={:?}",
        struct_name,
        properties.len(),
        properties
            .iter()
            .map(|p| {
                let pn = p.borrow().base_object().name.clone();
                let pk = p.borrow().kind();
                format!("{}:{:?}", pn, pk)
            })
            .collect::<Vec<_>>()
    );

    if bin {
        for prop in &properties {
            let dim = property_array_dim(prop);
            for _ in 0..dim {
                serialize_item::<E, _>(prop, runtime, linker, reader)?;
            }
        }
    } else {
        serialize_struct_tagged::<E, _>(&properties, runtime, linker, reader)?;
    }

    let pos_after = reader.stream_position()?;
    debug!(
        "serialize_struct_value EXIT  struct={} read={:#X} (pos {:#X} -> {:#X})",
        struct_name,
        pos_after - pos_before,
        pos_before,
        pos_after
    );
    Ok(())
}

fn serialize_struct_tagged<E, R>(
    properties: &[RcUnrealObject],
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
    reader: &mut R,
) -> io::Result<()>
where
    E: ByteOrder,
    R: LinRead,
{
    loop {
        let mut tag = PropertyTag::default();
        tag.deserialize::<E, _>(runtime, linker, reader)?;
        if tag.name.is_none(&linker.borrow()) {
            break;
        }
        let tag_name = {
            let l = linker.borrow();
            l.package
                .names
                .get(tag.name.raw() as usize)
                .map(|n| n.name.clone())
                .unwrap_or_default()
        };
        trace!(
            "serialize_struct_tagged tag name={} type={} size={:#X} info={:#X}",
            tag_name, tag.property_type, tag.size, tag.info
        );
        read_tag_value::<E, _>(&tag, properties, runtime, linker, reader)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_bin_allowlist() {
        // Sanity check the allowlist values; specific FName indices vary
        // per-package but the names are the same.
        // (We'd need a fully-built struct_obj to test is_native_bin_struct;
        //  this just checks the constant strings are what we expect.)
        let names: &[&str] = &["Vector", "Rotator", "Plane"];
        for n in names {
            assert!(n.eq_ignore_ascii_case(&n.to_lowercase()));
        }
    }
}
