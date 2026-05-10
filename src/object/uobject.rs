use std::{
    cell::RefCell,
    io,
    rc::{Rc, Weak},
};

use byteorder::ByteOrder;
use tracing::{Level, debug, event, span, trace};

use crate::{
    de::{ExportIndex, Linker, ObjectExport, RcLinker, WeakLinker},
    object::{
        DeserializeUnrealObject, ObjectFlags, RcUnrealObject, UObjectKind, UnrealObject,
        WeakUnrealObject,
        internal::{property::PropertyTag, serialize_item},
    },
    reader::{LinRead, UnrealReadExt},
    runtime::{LoadKind, UnrealRuntime},
};

#[derive(Debug)]
pub struct Object {
    pub name: String,
    pub flags: ObjectFlags,
    pub concrete_object_kind: Option<UObjectKind>,
    pub needs_load: bool,
    pub needs_post_load: bool,
    pub linker: Option<WeakLinker>,
    pub export_index: Option<ExportIndex>,
    pub outer_object: Option<RcUnrealObject>,
    pub concrete_obj: Option<WeakUnrealObject>,
    /// Monotonic counter set the first time the object is constructed by the
    /// runtime. Mirrors UE2's allocator-determined `ULinker*` ordering: the
    /// engine's `EndLoad` sort uses raw `ULinker*` pointer values, which in
    /// practice tracks "construction order" closely; we use an explicit
    /// counter so behaviour is reproducible.
    pub construction_index: u64,
    /// Populated only when `flags & HAS_STACK` is set. Mirrors SC's
    /// `FStateFrame` per `sub_46d40` allocation in xbe `UObject::Serialize`:
    /// `Node`, `StateNode`, `ProbeMask` (QWORD), `LatentAction` (DWORD),
    /// and a script-bytecode offset (`Code - Node->Script(0)`, AR_INDEX).
    pub state_frame: Option<FStateFrame>,
}

/// Mirrors SC's per-actor script execution stack frame, allocated at the
/// `"ObjectStateFrame"` site in `UObject::Serialize` (xbe `0x4858d`).
/// Only the four/five fields actually serialized are tracked here; the
/// in-memory class has more (`StateModifier`, `Probes`, etc.) that are
/// populated at runtime, not from disk.
#[derive(Default, Debug, Clone)]
pub struct FStateFrame {
    pub node: Option<RcUnrealObject>,
    pub state_node: Option<RcUnrealObject>,
    /// 8 raw bytes (QWORD probe mask).
    pub probe_mask: [u8; 8],
    pub latent_action: u32,
    /// `Code - Node->Script(0)` packed_int. `-1` (sentinel) when no
    /// active code pointer; otherwise a byte offset into `Node`'s
    /// `Script` array.
    pub code_offset: i32,
}

impl Default for Object {
    fn default() -> Self {
        Self {
            name: "None".to_owned(),
            flags: ObjectFlags::empty(),
            concrete_object_kind: None,
            needs_load: true,
            needs_post_load: true,
            linker: Default::default(),
            export_index: Default::default(),
            outer_object: None,
            concrete_obj: None,
            construction_index: 0,
            state_frame: None,
        }
    }
}

impl Object {
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn flags(&self) -> ObjectFlags {
        self.flags
    }

    pub fn set_flags(&mut self, flags: ObjectFlags) {
        self.flags = flags;
    }

    pub fn set_concrete_object_kind(&mut self, kind: UObjectKind) {
        self.concrete_object_kind = Some(kind);
    }

    pub fn concrete_object_kind(&self) -> UObjectKind {
        self.concrete_object_kind.expect("object_kind not set")
    }

    pub fn needs_load(&self) -> bool {
        self.needs_load
    }

    pub fn loaded(&mut self) {
        self.needs_load = false;
    }

    pub fn needs_post_load(&self) -> bool {
        self.needs_post_load
    }

    pub fn post_loaded(&mut self) {
        self.needs_post_load = false;
    }

    pub fn is_fully_loaded(&self) -> bool {
        !self.needs_load() && !self.needs_post_load()
    }

    pub fn set_linker(&mut self, linker: WeakLinker) {
        assert!(self.linker.is_none());

        self.linker = Some(linker);
    }

    pub fn linker(&self) -> RcLinker {
        self.linker
            .as_ref()
            .expect("linker is not set")
            .upgrade()
            .expect("could not upgrade WeakLinker")
    }

    pub fn set_export_index(&mut self, export_index: ExportIndex) {
        assert!(self.export_index.is_none());

        self.export_index = Some(export_index);
    }

    pub fn export_index(&self) -> ExportIndex {
        self.export_index.expect("export_index is not set")
    }

    pub fn set_outer_object(&mut self, outer: RcUnrealObject) {
        self.outer_object = Some(outer);
    }

    pub fn outer_object(&self) -> Option<&RcUnrealObject> {
        self.outer_object.as_ref()
    }

    pub fn set_concrete_obj(&mut self, outer: WeakUnrealObject) {
        self.concrete_obj = Some(outer);
    }

    pub fn concrete_obj(&self) -> RcUnrealObject {
        self.concrete_obj
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .expect("concrete object pointer was never set or died")
    }

    pub fn set_construction_index(&mut self, idx: u64) {
        self.construction_index = idx;
    }

    pub fn construction_index(&self) -> u64 {
        self.construction_index
    }
}

impl DeserializeUnrealObject for Object {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_object");
        let _enter = span.enter();

        debug!(
            "Deserializing UObject for object with kind {:?}",
            self.concrete_object_kind
        );

        if self.flags.contains(ObjectFlags::HAS_STACK) {
            // Mirrors xbe `UObject::Serialize` HAS_STACK block at `0x4859c`:
            //   Ar << Node;            // packed_int (UObject*)
            //   Ar << StateNode;       // packed_int (UObject*)
            //   Ar.Serialize(&ProbeMask, 8);
            //   Ar.Serialize(&LatentAction, 4);
            //   if (Node) {
            //       Ar.Preload(Node);                       // seek+body+seek-back
            //       Ar << AR_INDEX(Code - Node->Script(0)); // packed_int offset
            //   }
            let node = reader.read_object::<E>(runtime, linker)?;
            let state_node = reader.read_object::<E>(runtime, linker)?;
            let mut probe_mask = [0u8; 8];
            reader.cheat(&mut probe_mask)?;
            let mut latent_buf = [0u8; 4];
            reader.cheat(&mut latent_buf)?;
            let latent_action = u32::from_le_bytes(latent_buf);
            let code_offset = if let Some(node_obj) = node.clone() {
                runtime.full_load_object::<E, _>(&node_obj, reader)?;
                reader.read_packed_int()?
            } else {
                -1
            };
            self.state_frame = Some(FStateFrame {
                node,
                state_node,
                probe_mask,
                latent_action,
                code_offset,
            });
        }

        if self.concrete_object_kind() != UObjectKind::Class {
            // Resolve this instance's class so the tag loop can
            // dispatch SerializeItem on `tag.name`-matched property
            // children. `load_object_by_raw_index` handles both
            // cases: export class (positive class_index, this linker)
            // and import class (negative, walks to the linker that
            // owns the class). Returns None only when the class is a
            // native UClass we don't model (resolved via the engine's
            // `StaticFindObject` C++ table) — those collapse to an
            // empty chain and the tag loop falls through to the
            // engine's `SerializeTaggedProperties` skip-on-mismatch
            // (cheat tag.size bytes). Same behavior as the engine
            // when the property dispatch can't resolve.
            let class_obj = resolve_instance_class::<E, _>(self, runtime, linker, reader)?;
            let properties = match class_obj.as_ref() {
                Some(c) => serialize_item::collect_struct_properties(c),
                None => Vec::new(),
            };
            debug!(
                "Object::deserialize: instance class chain len={} (class_resolved={})",
                properties.len(),
                class_obj.is_some()
            );

            let mut tags = Vec::new();
            loop {
                trace!("Deserializing property");
                let mut tag = PropertyTag::default();
                tag.deserialize::<E, _>(runtime, linker, reader)?;

                if tag.name.is_none(&linker.borrow()) {
                    break;
                }

                read_tag_value::<E, _>(&tag, &properties, runtime, linker, reader)?;

                tags.push(tag);
            }
        }

        Ok(())
    }
}

fn resolve_instance_class<E, R>(
    obj: &Object,
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
    reader: &mut R,
) -> io::Result<Option<RcUnrealObject>>
where
    E: ByteOrder,
    R: LinRead,
{
    let class_index = {
        let l = linker.borrow();
        let Some(export) = l.find_export_by_index(obj.export_index()) else {
            return Ok(None);
        };
        export.class_index
    };
    if class_index == 0 {
        return Ok(None);
    }
    runtime.load_object_by_raw_index::<E, _>(class_index, linker, LoadKind::Create, reader)
}

/// Reads the value bytes for a property tag, mirroring SC's
/// `FPropertyTag::SerializeTaggedProperty` dispatch. Bool's value is in
/// `tag.info`'s high bit (zero bytes consumed); primitives serialize
/// inline; object refs route through `read_object`. StructProperty (10)
/// and ArrayProperty (9) dispatch to `serialize_item` via a name lookup
/// in `properties`. When the tag's name doesn't resolve in `properties`
/// (or `properties` is empty), Struct/Array fall back to consuming
/// `tag.size` raw bytes — matches the engine's forward-compat skip and
/// keeps trace replay aligned at the cost of dropping inner refs.
pub(crate) fn read_tag_value<E, R>(
    tag: &PropertyTag,
    properties: &[RcUnrealObject],
    runtime: &mut UnrealRuntime,
    linker: &Rc<RefCell<Linker>>,
    reader: &mut R,
) -> io::Result<()>
where
    E: ByteOrder,
    R: LinRead,
{
    use crate::reader::UnrealReadExt;
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
    match tag.property_type {
        NAME_BOOL_PROPERTY => {}
        NAME_OBJECT_PROPERTY | NAME_CLASS_PROPERTY | NAME_DELEGATE_PROPERTY => {
            let _ = reader.read_object::<E>(runtime, linker)?;
        }
        NAME_NAME_PROPERTY => {
            let _ = reader.read_packed_int()?;
        }
        NAME_INT_PROPERTY | NAME_FLOAT_PROPERTY => {
            let mut buf = [0u8; 4];
            reader.cheat(&mut buf)?;
        }
        NAME_BYTE_PROPERTY => {
            let mut buf = [0u8; 1];
            reader.cheat(&mut buf)?;
        }
        NAME_STRUCT_PROPERTY => {
            let tag_name = {
                let l = linker.borrow();
                l.package
                    .names
                    .get(tag.name.raw() as usize)
                    .map(|n| n.name.clone())
                    .unwrap_or_default()
            };
            let prop = serialize_item::find_property_by_name(properties, &tag_name);
            let struct_obj = prop.as_ref().and_then(|p| {
                p.borrow()
                    .as_any()
                    .downcast_ref::<crate::object::builtins::StructProperty>()
                    .and_then(|sp| sp.struct_obj.as_ref().cloned())
            });
            if let Some(s) = struct_obj {
                serialize_item::serialize_struct_value::<E, _>(&s, runtime, linker, reader)?;
            } else {
                debug!(
                    "read_tag_value: StructProperty tag {} unresolved in chain (chain_len={}); cheat({:#X})",
                    tag_name,
                    properties.len(),
                    tag.size
                );
                if tag.size > 0 {
                    let mut buf = vec![0u8; tag.size as usize];
                    reader.cheat(&mut buf)?;
                }
            }
        }
        NAME_ARRAY_PROPERTY => {
            let tag_name = {
                let l = linker.borrow();
                l.package
                    .names
                    .get(tag.name.raw() as usize)
                    .map(|n| n.name.clone())
                    .unwrap_or_default()
            };
            let prop = serialize_item::find_property_by_name(properties, &tag_name);
            let inner = prop.as_ref().and_then(|p| {
                p.borrow()
                    .as_any()
                    .downcast_ref::<crate::object::builtins::ArrayProperty>()
                    .and_then(|ap| ap.inner.as_ref().cloned())
            });
            if let Some(i) = inner {
                serialize_item::serialize_array_value::<E, _>(&i, runtime, linker, reader)?;
            } else {
                debug!(
                    "read_tag_value: ArrayProperty tag {} unresolved in chain (chain_len={}); cheat({:#X})",
                    tag_name,
                    properties.len(),
                    tag.size
                );
                if tag.size > 0 {
                    let mut buf = vec![0u8; tag.size as usize];
                    reader.cheat(&mut buf)?;
                }
            }
        }
        _ => {
            if tag.size > 0 {
                let mut buf = vec![0u8; tag.size as usize];
                reader.cheat(&mut buf)?;
            }
        }
    }
    Ok(())
}


impl UnrealObject for Object {
    fn kind(&self) -> UObjectKind {
        UObjectKind::Object
    }

    fn parent_object(&self) -> Option<&dyn UnrealObject> {
        None
    }

    fn base_object(&self) -> &Object {
        self
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn is_a(&self, kind: UObjectKind) -> bool {
        self.kind() == kind
    }

    fn parent_object_mut(&mut self) -> Option<&mut dyn UnrealObject> {
        None
    }

    fn base_object_mut(&mut self) -> &mut Object {
        self
    }

    fn parent_of_kind(&self, kind: UObjectKind) -> Option<&dyn UnrealObject> {
        if kind == UObjectKind::Object {
            Some(self)
        } else {
            None
        }
    }

    fn parent_of_kind_mut(&mut self, kind: UObjectKind) -> Option<&mut dyn UnrealObject> {
        if kind == UObjectKind::Object {
            Some(self)
        } else {
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::{UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Object].iter().cloned()
    }

    #[test]
    fn test_is_a() {
        let test_obj = Object::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
