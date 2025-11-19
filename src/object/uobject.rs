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
    /// The concrete type of this object
    pub concrete_object_kind: Option<UObjectKind>,
    pub needs_load: bool,
    pub needs_post_load: bool,
    pub linker: Option<WeakLinker>,
    pub export_index: Option<ExportIndex>,
    pub outer_object: Option<RcUnrealObject>,
    pub concrete_obj: Option<WeakUnrealObject>,
    // package_index: usize,
    // class: i32,
    // outer: i32, //RcUnrealObject,
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

                todo!("Tagged properties");

                properties.push(tag);
            }
        }

        Ok(())
    }
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
