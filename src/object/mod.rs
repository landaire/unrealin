/// Internal types that are not directly exposed to the scripting engine
mod internal;
#[cfg(test)]
mod test_common;
mod uclass;
mod ufield;
mod ufunction;
mod uobject;
mod uproperty;
mod ustate;
mod ustruct;
mod utext_buffer;

use std::cell::RefCell;
use std::io::{self, Read, Seek};
use std::rc::Rc;
use tracing::trace;

const NAME_NONE: usize = 0;

use bitflags::bitflags;
use byteorder::ByteOrder;
use paste::paste;
pub mod builtins {
    pub use super::uclass::Class;
    pub use super::ufield::Field;
    pub use super::ufunction::Function;
    pub use super::uobject::Object;
    pub use super::uproperty::*;
    pub use super::ustate::State;
    pub use super::ustruct::Struct;
    pub use super::utext_buffer::TextBuffer;
}

use builtins::*;

use crate::de::{ExportIndex, Linker, ObjectExport, RcLinker, WeakLinker};
use crate::reader::LinRead;
use crate::runtime::UnrealRuntime;

pub type RcUnrealObject = Rc<RefCell<dyn UnrealObject>>;

pub trait UnrealObject: std::fmt::Debug {
    fn kind(&self) -> UObjectKind;
    fn parent_object(&self) -> Option<&dyn UnrealObject>;
    fn parent_object_mut(&mut self) -> Option<&mut dyn UnrealObject>;
    fn base_object(&self) -> &Object;
    fn base_object_mut(&mut self) -> &mut Object;
    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
    fn is_a(&self, kind: UObjectKind) -> bool;
    fn parent_of_kind(&self, kind: UObjectKind) -> Option<&dyn UnrealObject>;
    fn parent_of_kind_mut(&mut self, kind: UObjectKind) -> Option<&mut dyn UnrealObject>;
}

pub trait DeserializeUnrealObject {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: ByteOrder,
        R: LinRead;
}

macro_rules! register_builtins {
    ($($name:ident),*) => {
        #[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub enum UObjectKind {
            $(
                $name,
            )*
        }

        impl UObjectKind {
            const fn all() -> &'static [UObjectKind] {
                [
                    $(
                        UObjectKind::$name,
                    )*
                ].as_slice()
            }

            pub fn construct(&self, linker: WeakLinker, export_index: ExportIndex) -> Rc<RefCell<dyn UnrealObject>>  {
                match self {
                    $(
                        Self::$name => {
                            let mut obj = $name::default();
                            {
                                let mut base = obj.base_object_mut();
                                base.set_concrete_object_kind(UObjectKind::$name);
                                base.set_linker(linker);
                                base.set_export_index(export_index);
                            }


                            Rc::new(RefCell::new(obj))
                        }
                    )*
                }
            }

            pub fn as_str(&self) -> &'static str {
                match self {
                    $(
                        Self::$name => {
                            stringify!($name)
                        }
                    )*
                }
            }

            $(
              paste! {
                  pub fn [<is_ $name:lower>](&self) -> bool {
                      matches!(self, UObjectKind::$name)
                  }
              }
            )*
        }

        impl TryFrom<&str> for UObjectKind {
            type Error = ();

            fn try_from(name: &str) -> Result<Self, Self::Error> {
                match name {
                    $(
                        stringify!($name) => {
                            Ok(UObjectKind::$name)
                        }
                    )*
                    _ => {
                        Err(())
                    }
                }
            }
        }


        pub(crate) fn deserialize_object<E, R>(
            runtime: &mut UnrealRuntime,
            object: RcUnrealObject,
            linker: &RcLinker,
            reader: &mut R,
        ) -> io::Result<()>
        where
            R: LinRead,
            E: ByteOrder,
        {
            let object_kind = object.borrow().kind();

            match object_kind {
                $(
                    UObjectKind::$name => {
                        let mut object = object.borrow_mut();

                        let concrete_ty = object
                            .as_any_mut()
                            .downcast_mut::<$name>()
                            .unwrap_or_else(|| panic!("failed to cast to {}", stringify!($kind)));

                        concrete_ty.deserialize::<E, _>(runtime, linker, reader)
                    }
                )*
            }
        }
    };
}

register_builtins!(
    Object,
    Struct,
    State,
    Class,
    Field,
    TextBuffer,
    Function,
    Property,
    FloatProperty,
    StrProperty,
    BoolProperty,
    ObjectProperty,
    ClassProperty
);

macro_rules! make_inherited_object {
    ($($name:ident),*) => {
        $(
            impl UnrealObject for $name {
                fn kind(&self) -> UObjectKind {
                    UObjectKind::$name
                }

                fn parent_object(&self) -> Option<&dyn UnrealObject> {
                    Some(&self.parent_object)
                }

                fn parent_object_mut(&mut self) -> Option<&mut dyn UnrealObject> {
                    Some(&mut self.parent_object)
                }

                fn base_object(&self) -> &Object {
                    let mut current_object = self.parent_object().expect("current_object has no ParentObject");
                    while current_object.kind() != UObjectKind::Object {
                        current_object = current_object.parent_object().expect("current_object has no ParentObject");
                    }

                    current_object.as_any().downcast_ref::<Object>().expect("base object is not an Object")
                }

                fn base_object_mut(&mut self) -> &mut Object {
                    let mut current_object = self.parent_object_mut().expect("current_object has no ParentObject");
                    while current_object.kind() != UObjectKind::Object {
                        current_object = current_object.parent_object_mut().expect("current_object has no ParentObject");
                    }

                    current_object.as_any_mut().downcast_mut::<Object>().expect("base object is not an Object")
                }

                fn as_any(&self) -> &dyn std::any::Any {
                    self
                }

                fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                    self
                }

                fn is_a(&self, kind: UObjectKind) -> bool {
                    self.parent_of_kind(kind).is_some()
                }

                fn parent_of_kind(&self, kind: UObjectKind) -> Option<&dyn UnrealObject> {
                    let mut current_object = self as &dyn UnrealObject;
                    if current_object.kind() == kind {
                        return Some(current_object);
                    }

                    while let Some(parent) = current_object.parent_object() {
                        if parent.kind() == kind {
                            return Some(parent);
                        }

                        current_object = parent;
                    }

                    None
                }

                fn parent_of_kind_mut(&mut self, kind: UObjectKind) -> Option<&mut dyn UnrealObject> {
                    let mut current_object = self as &mut dyn UnrealObject;
                    if current_object.kind() == kind {
                        return Some(current_object);
                    }

                    while let Some(parent) = current_object.parent_object_mut() {
                        if parent.kind() == kind {
                            return Some(parent);
                        }

                        current_object = parent;
                    }

                    None
                }
            }
        )*
    };
}

make_inherited_object!(
    Struct,
    State,
    Class,
    Field,
    TextBuffer,
    Function,
    Property,
    FloatProperty,
    StrProperty,
    BoolProperty,
    ObjectProperty,
    ClassProperty
);

bitflags! {
    #[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
    pub struct ObjectFlags: u32 {
        /// Object is transactional.
        const TRANSACTIONAL    = 0x00000001;
        /// Object is not reachable on the object graph.
        const UNREACHABLE		= 0x00000002;
        /// Object is visible outside its package.
        const PUBLIC			= 0x00000004;
        /// Temporary import tag in load/save.
        const TAG_IMP			= 0x00000008;
        /// Temporary export tag in load/save.
        const TAG_EXP			= 0x00000010;
        /// Modified relative to source files.
        const SOURCE_MODIFIED   = 0x00000020;
        /// Check during garbage collection.
        const TAG_GARBAGE		= 0x00000040;
        /// Object is not visible outside of class.
        const FINAL			= 0x00000080;
        /// Object is localized by instance name, not by class.
        const PER_OBJECT_LOCALIZED=0x00000100;
        /// During load, indicates object needs loading.
        const NEED_LOAD			= 0x00000200;
        /// A hardcoded name which should be syntax-highlighted.
        const HIGHLIGHTED_NAME  = 0x00000400;
        /// NULL out references to this during garbage collecion.
        const ELIMINATE_OBJECT  = 0x00000400;
        /// In a singular function.
        const IN_SINGULAR_FUNC   = 0x00000800;
        /// Name is remapped.
        const REMAPPED_NAME     = 0x00000800;
        /// Property is protected (may only be accessed from its owner class or subclasses)
        const PROTECTED        = 0x00000800;
        /// warning: Mirrored in UnName.h. Suppressed log name.
        const SUPPRESS         = 0x00001000;
        /// Object did a state change.
        const STATE_CHANGED     = 0x00001000;
        /// Within an EndState call.
        const IN_END_STATE       = 0x00002000;
        /// Don't save object.
        const TRANSIENT        = 0x00004000;
        /// Data is being preloaded from file.
        const PRELOADING       = 0x00008000;
        /// In-file load for client.
        const LOAD_FOR_CLIENT	= 0x00010000;
        /// In-file load for client.
        const LOAD_FOR_SERVER	= 0x00020000;
        /// In-file load for client.
        const LOAD_FOR_EDIT		= 0x00040000;
        /// Keep object around for editing even if unreferenced.
        const STANDALONE       = 0x00080000;
        /// Don't load this object for the game client.
        const NOT_FOR_CLIENT		= 0x00100000;
        /// Don't load this object for the game server.
        const NOT_FOR_SERVER		= 0x00200000;
        /// Don't load this object for the editor.
        const NOT_FOR_EDIT		= 0x00400000;
        /// Object Destroy has already been called.
        const DESTROYED        = 0x00800000;
        /// Object needs to be postloaded.
        const NEED_POST_LOAD		= 0x01000000;
        /// Has execution stack.
        const HAS_STACK         = 0x02000000;
        /// Native (UClass only).
        const NATIVE			= 0x04000000;
        /// Marked (for debugging).
        const MARKED			= 0x08000000;
        /// ShutdownAfterError called.
        const ERROR_SHUTDOWN    = 0x10000000;
        /// For debugging Serialize calls.
        const DEBUG_POST_LOAD    = 0x20000000;
        /// For debugging Serialize calls.
        const DEBUG_SERIALIZE   = 0x40000000;
        /// For debugging Destroy calls.
        const DEBUG_DESTROY     = 0x80000000;
    }
}
