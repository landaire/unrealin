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
        DeserializeUnrealObject, NAME_NONE, ObjectFlags, RcUnrealObject, UObjectKind, UnrealObject,
        WeakUnrealObject, internal::property::PropertyTag,
    },
    reader::LinRead,
    runtime::UnrealRuntime,
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
            todo!("UObject HAS_STACK path");
        }

        if self.concrete_object_kind() != UObjectKind::Class {
            let mut properties = Vec::new();
            loop {
                trace!("Deserializing property");
                let mut tag = PropertyTag::default();
                tag.deserialize::<E, _>(runtime, linker, reader)?;

                if tag.name.is_none() {
                    break;
                }

                read_tag_value::<E, _>(&tag, runtime, linker, reader)?;

                properties.push(tag);
            }
        }

        Ok(())
    }
}

/// Reads the value bytes for a property tag, mirroring
/// `FPropertyTag::SerializeTaggedProperty`. For object refs we use
/// `read_object` so `IndexToObject`-style cascade fires; for primitives we
/// consume the right number of bytes via `cheat()`.
pub(crate) fn read_tag_value<E, R>(
    tag: &PropertyTag,
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
