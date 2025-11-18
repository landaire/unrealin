use std::{cell::RefCell, rc::Rc};

use crate::{
    object::{DeserializeUnrealObject, RcUnrealObject, internal::fname::FName, ufield::Field},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};
use bitflags::bitflags;
use byteorder::ReadBytesExt;
use serde::Serialize;
use tracing::{Level, span, trace};

#[derive(Default, Debug)]
pub struct Property {
    pub parent_object: Field,

    array_dim: u16,
    element_size: u32,
    property_flags: PropertyFlags,
    category: FName,
    rep_offset: u16,
    rep_index: u16,
    comment_string: Option<String>,
}

impl Property {
    pub fn flags(&self) -> PropertyFlags {
        self.property_flags
    }
}

impl DeserializeUnrealObject for Property {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<crate::de::Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_property");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        trace!("array_dim");
        // TODO: This is only for splinter cell?
        self.array_dim = reader.read_u16::<E>()?;
        trace!("property_flags");
        self.property_flags = PropertyFlags::from_bits(reader.read_u32::<E>()?)
            .expect("failed to parse property flags");
        trace!("category");
        self.category.deserialize::<E, _>(runtime, linker, reader)?;

        if self.property_flags.contains(PropertyFlags::NET) {
            self.rep_offset = reader.read_u16::<E>()?;
        }

        if self.property_flags.contains(PropertyFlags::COMMENT_STRING) {
            self.comment_string = Some(reader.read_string()?);
        }

        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct FloatProperty {
    pub parent_object: Property,
}

impl DeserializeUnrealObject for FloatProperty {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<crate::de::Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_float");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct StrProperty {
    pub parent_object: Property,
}

impl DeserializeUnrealObject for StrProperty {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<crate::de::Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_str_property");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct BoolProperty {
    pub parent_object: Property,
}

impl DeserializeUnrealObject for BoolProperty {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<crate::de::Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_bool_property");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct ObjectProperty {
    pub parent_object: Property,

    pub property_class: Option<RcUnrealObject>,
}

impl DeserializeUnrealObject for ObjectProperty {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<crate::de::Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_object_property");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        self.property_class = reader.read_object::<E>(runtime, linker)?;

        Ok(())
    }
}

#[derive(Default, Debug)]
pub struct ClassProperty {
    pub parent_object: ObjectProperty,

    pub meta_class: Option<RcUnrealObject>,
}

impl DeserializeUnrealObject for ClassProperty {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<crate::de::Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_class_property");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        self.meta_class = reader.read_object::<E>(runtime, linker)?;

        Ok(())
    }
}

bitflags! {
    /// Flags associated with each property in a class, overriding the
    /// property's default behavior.
    #[derive(Default, Clone, Copy, Debug)]
    pub struct PropertyFlags: u32 {
        /// Property is user-settable in the editor.
        const EDIT = 0x00000001;
        /// Actor's property always matches class's default actor property.
        const CONST = 0x00000002;
        /// Variable is writable by the input system.
        const INPUT = 0x00000004;
        /// Object can be exported with actor.
        const EXPORT_OBJECT = 0x00000008;
        /// Optional parameter (if CPF_Param is set).
        const OPTIONAL_PARM = 0x00000010;
        /// Property is relevant to network replication.
        const NET = 0x00000020;
        /// Prevent adding/removing of items from dynamic a array in the editor.
        const EDIT_CONST_ARRAY = 0x00000040;
        /// Function/When call parameter.
        const PARM = 0x00000080;
        /// Value is copied out after function call.
        const OUT_PARM = 0x00000100;
        /// Property is a short-circuitable evaluation function parm.
        const SKIP_PARM = 0x00000200;
        /// Return value.
        const RETURN_PARM = 0x00000400;
        /// Coerce args into this function parameter.
        const COERCE_PARM = 0x00000800;
        /// Property is native: C++ code is responsible for serializing it.
        const NATIVE = 0x00001000;
        /// Property is transient: shouldn't be saved, zero-filled at load time.
        const TRANSIENT = 0x00002000;
        /// Property should be loaded/saved as permanent profile.
        const CONFIG = 0x00004000;
        /// Property should be loaded as localizable text.
        const LOCALIZED = 0x00008000;
        /// Property travels across levels/servers.
        const TRAVEL = 0x00010000;
        /// Property is uneditable in the editor.
        const EDIT_CONST = 0x00020000;
        /// Load config from base class, not subclass.
        const GLOBAL_CONFIG = 0x00040000;
        /// Object or dynamic array loaded on demand only.
        const ON_DEMAND = 0x00100000;
        /// Automatically create inner object.
        const NEW = 0x00200000;
        /// Fields need construction/destruction.
        const NEED_CTOR_LINK = 0x00400000;
        /// Property should not be exported to the native class header file.
        const NO_EXPORT = 0x00800000;
        /// String that has "Go" button which allows it to call functions from UEd.
        const BUTTON = 0x01000000;
        /// Property should be included when cache is exported (02/25/04 - for now, this is only used in localization exporting code, but will eventually be used to support custom cache props)
        const CACHE = 0x01000000;
        /// Property has a comment string visible via the property browser
        const COMMENT_STRING = 0x02000000;
        /// Edit this object reference inline.
        const EDIT_INLINE = 0x04000000;
        /// References are set by clicking on actors in the editor viewports.
        const ED_FINDABLE = 0x08000000;
        /// EditInline with Use button.
        const EDIT_INLINE_USE = 0x10000000;
        /// Property is deprecated.  Read it from an archive, but don't save it.
        const DEPRECATED = 0x20000000;
        /// EditInline, notify outer object on editor change.
        const EDIT_INLINE_NOTIFY = 0x40000000;
        /// This property can be automated (for GUI)
        const AUTOMATED = 0x80000000;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Property]
            .iter()
            .cloned()
            .chain(crate::object::ufield::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = Property::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
