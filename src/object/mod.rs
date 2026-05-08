/// Internal types that are not directly exposed to the scripting engine
pub(crate) mod internal;
#[cfg(test)]
mod test_common;
mod uaudio_subsystem;
mod uclass;
mod uclient;
mod uconst;
mod ucylinder;
mod uenum;
mod ufield;
mod ufont;
mod ufunction;
mod ulanguage;
pub(crate) mod uobject;
mod upackage;
mod upalette;
mod upolys;
mod uprimitive;
mod uproperty;
mod urenderdevice;
mod usound;
mod ustate;
mod ulevel;
mod ulevel_base;
mod ulod_mesh;
mod umesh;
mod umodel;
mod uskel_mesh;
mod ustatic_mesh;
mod ustruct;
mod usubsystem;
mod utext_buffer;
mod utexture;

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::{Rc, Weak};
use tracing::Level as TracingLevel;
use tracing::span;
use tracing::trace;

use bitflags::bitflags;
use byteorder::ByteOrder;
use paste::paste;
pub mod builtins {
    pub use super::uaudio_subsystem::AudioSubsystem;
    pub use super::uclass::Class;
    pub use super::uclient::Client;
    pub use super::uconst::Const;
    pub use super::ucylinder::Cylinder;
    pub use super::uenum::Enum;
    pub use super::ufield::Field;
    pub use super::ufont::Font;
    pub use super::ufunction::Function;
    pub use super::ulanguage::Language;
    pub use super::uobject::Object;
    pub use super::upackage::Package;
    pub use super::upalette::Palette;
    pub use super::upolys::Polys;
    pub use super::uprimitive::Primitive;
    pub use super::uproperty::{
        ArrayProperty, BoolProperty, ByteProperty, ClassProperty, FloatProperty, IntProperty, Link,
        NameProperty, ObjectProperty, Property, PropertyFlags, StrProperty, StructProperty,
    };
    pub use super::ulevel::Level;
    pub use super::ulevel_base::LevelBase;
    pub use super::ulod_mesh::LodMesh;
    pub use super::umesh::Mesh;
    pub use super::umodel::Model;
    pub use super::urenderdevice::RenderDevice;
    pub use super::uskel_mesh::SkeletalMesh;
    pub use super::usound::Sound;
    pub use super::ustate::State;
    pub use super::ustatic_mesh::StaticMesh;
    pub use super::ustruct::Struct;
    pub use super::usubsystem::Subsystem;
    pub use super::utext_buffer::TextBuffer;
    pub use super::utexture::Texture;
}

use builtins::*;

use crate::de::{ExportIndex, Linker, ObjectExport, RcLinker, WeakLinker};
use crate::reader::LinRead;
use crate::runtime::UnrealRuntime;

/// How an export's body bytes were produced for re-emit. Drives the
/// per-class story for moving off captured-bytes-verbatim:
///
/// * `Opaque` is the captured raw stream from `preload`. Used for
///   classes whose load/save are byte-identical AND for classes we
///   haven't yet stood up a per-field re-emit path for. The bytes go
///   straight to disk; correctness depends on the original LIN.
///
/// * `Reconstructed` is bytes produced by walking the parsed Rust
///   fields and re-emitting them via canonical encoders. Correctness
///   depends on the deserialize path having captured every field
///   losslessly. Required wherever load and save diverge (e.g.
///   `UStruct::Serialize` reading ScriptText into a discarded local;
///   the `handle_optional_debug_info` peek-back contaminating the
///   captured stream with phantom bytes).
///
/// * `Patched` is captured/reconstructed bytes plus a set of
///   body-relative offsets to patch at write time. Used for fields
///   that hold absolute file offsets pointing INSIDE the same body
///   (e.g. UTexture mipmap `SeekPos` values, which point past the
///   mip's lazy data). The original bytes have stale absolute
///   offsets baked in; ser.rs computes each new offset as
///   `body_file_position + target_offset_within_body` and overwrites
///   the 4 bytes at `body_offset` before flushing the body.
#[derive(Debug, Clone)]
pub enum BodyKind {
    Opaque(Vec<u8>),
    Reconstructed(Vec<u8>),
    Patched {
        bytes: Vec<u8>,
        patches: Vec<BodyOffsetPatch>,
    },
}

/// A 4-byte little-endian u32 inside a body whose value is an
/// absolute file offset. `target_offset_within_body` is the position
/// (relative to the body's first byte) that the patched value should
/// resolve to. ser.rs writes
/// `body_file_position + target_offset_within_body` as a LE u32 at
/// `body[body_offset..body_offset + 4]`.
#[derive(Debug, Clone, Copy)]
pub struct BodyOffsetPatch {
    pub body_offset: usize,
    pub target_offset_within_body: usize,
}

impl BodyKind {
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            BodyKind::Opaque(b) | BodyKind::Reconstructed(b) => b,
            BodyKind::Patched { bytes, .. } => bytes,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            BodyKind::Opaque(b) | BodyKind::Reconstructed(b) => b,
            BodyKind::Patched { bytes, .. } => bytes,
        }
    }

    pub fn is_reconstructed(&self) -> bool {
        matches!(self, BodyKind::Reconstructed(_))
    }
}

pub trait SerializeUnrealObject {
    /// Produce the body bytes for this export. The default returns
    /// `BodyKind::Opaque(captured)` (captured raw stream verbatim).
    /// Override to return `BodyKind::Reconstructed(...)` once a
    /// per-field re-emit is wired up; the dispatcher in
    /// `serialize_object` will route the call to the override.
    fn serialize<E>(
        &self,
        _linker: &Linker,
        _export_index: ExportIndex,
        captured: &[u8],
    ) -> io::Result<BodyKind>
    where
        E: ByteOrder,
    {
        Ok(BodyKind::Opaque(captured.to_vec()))
    }
}

pub type RcUnrealObject = Rc<RefCell<dyn UnrealObject>>;
pub type WeakUnrealObject = Weak<RefCell<dyn UnrealObject>>;

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
        linker: &RcLinker,
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

            pub fn construct(&self, linker: WeakLinker, export_index: ExportIndex) -> RcUnrealObject  {
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
            let span = span!(TracingLevel::DEBUG,
                "deserialize_object",
                obj_ptr = format!("{:#x}", object.as_ptr().expose_provenance())
            );
            let _enter = span.enter();
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

        /// Polymorphic dispatch from `&dyn UnrealObject` to the type's
        /// `SerializeUnrealObject::serialize` impl. Each type gets the
        /// default `BodyKind::Opaque(captured)` unless it has a custom
        /// `impl SerializeUnrealObject` overriding `serialize` (e.g.
        /// `ustruct.rs` splices canonical script bytes).
        pub(crate) fn serialize_object<E>(
            object: &dyn UnrealObject,
            linker: &Linker,
            export_index: ExportIndex,
            captured: &[u8],
        ) -> io::Result<BodyKind>
        where
            E: ByteOrder,
        {
            match object.kind() {
                $(
                    UObjectKind::$name => {
                        let concrete_ty = object
                            .as_any()
                            .downcast_ref::<$name>()
                            .unwrap_or_else(|| panic!("failed to cast to {}", stringify!($name)));

                        concrete_ty.serialize::<E>(linker, export_index, captured)
                    }
                )*
            }
        }
    };
}

/// Bulk-impls `SerializeUnrealObject` with the trait's default body for
/// every type that doesn't need a custom serialize. Types with custom
/// re-emit (e.g. `Struct`'s script-section splice) are intentionally
/// omitted here and provide their own `impl SerializeUnrealObject`.
macro_rules! impl_default_serialize {
    ($($name:ident),* $(,)?) => {
        $(
            impl SerializeUnrealObject for $name {}
        )*
    };
}

register_builtins!(
    Object,
    Struct,
    State,
    Class,
    Field,
    Const,
    TextBuffer,
    Function,
    Property,
    FloatProperty,
    StrProperty,
    BoolProperty,
    ObjectProperty,
    ClassProperty,
    IntProperty,
    NameProperty,
    StructProperty,
    ByteProperty,
    ArrayProperty,
    Enum,
    Font,
    Texture,
    Palette,
    Sound,
    Package,
    Primitive,
    StaticMesh,
    Mesh,
    LodMesh,
    SkeletalMesh,
    LevelBase,
    Level,
    Model,
    Polys
);

// Bulk-impl `SerializeUnrealObject` with the trait's default (Opaque
// captured bytes) for every type that doesn't have a custom serialize.
// Absent here:
//   - `Struct` (custom impl in `ustruct.rs`: splices canonical script
//     bytes into the body)
//   - `Function`, `State`, `Class` (UStruct subtypes; their impls in
//     their respective files delegate to Struct's serialize so the
//     splice covers function/state/class bodies too)
//   - `Texture` (custom impl in `utexture.rs`: patches stale absolute
//     mipmap `SeekPos` offsets to their new positions on re-emit)
//   - `StaticMesh` (custom impl in `ustatic_mesh.rs`: same TLazyArray
//     pattern as Texture; patches the lazy-body skip offset)
impl_default_serialize!(
    Object,
    Field,
    Const,
    TextBuffer,
    Property,
    FloatProperty,
    StrProperty,
    BoolProperty,
    ObjectProperty,
    ClassProperty,
    IntProperty,
    NameProperty,
    StructProperty,
    ByteProperty,
    ArrayProperty,
    Enum,
    Font,
    Palette,
    Sound,
    Package,
    Primitive,
    Mesh,
    LodMesh,
    SkeletalMesh,
    LevelBase,
    Level,
    Model,
    Polys,
);

macro_rules! make_inherited_objects {
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

make_inherited_objects!(
    Struct,
    State,
    Class,
    Field,
    Const,
    TextBuffer,
    Function,
    Property,
    FloatProperty,
    StrProperty,
    BoolProperty,
    ObjectProperty,
    ClassProperty,
    IntProperty,
    NameProperty,
    StructProperty,
    ByteProperty,
    ArrayProperty,
    Enum,
    Font,
    Texture,
    Palette,
    Sound,
    Package,
    Primitive,
    StaticMesh,
    Mesh,
    LodMesh,
    SkeletalMesh,
    LevelBase,
    Level,
    Model,
    Polys
);

macro_rules! register_linkable {
    ($($name:ident),*) => {
        pub(crate) fn link_object<E, R>(
            runtime: &mut UnrealRuntime,
            object: RcUnrealObject,
            linker: &RcLinker,
            reader: &mut R,
        ) -> io::Result<()>
        where
            R: LinRead,
            E: ByteOrder,
        {
            let span = span!(TracingLevel::DEBUG, "link_object",
                obj_ptr = format!("{:#x}", object.as_ptr().expose_provenance())
            );
            let _enter = span.enter();

            let object = object.borrow();
            let object_kind = object.kind();

            match object_kind {
                $(
                    UObjectKind::$name => {
                        let concrete_ty = object
                            .as_any()
                            .downcast_ref::<$name>()
                            .unwrap_or_else(|| panic!("failed to cast to {}", stringify!($name)));
                        concrete_ty.link::<E, R>(runtime, linker, reader)
                    }
                )*
                _ => {
                    if object_kind.as_str().ends_with("Property") {
                        panic!("{object_kind:?} should probably support linking?");
                    }

                     // Types that don't implement Link just return Ok
                    Ok(())
                },
            }
        }
    };
}

register_linkable!(
    FloatProperty,
    StrProperty,
    BoolProperty,
    IntProperty,
    NameProperty,
    ObjectProperty,
    ClassProperty,
    StructProperty,
    ByteProperty,
    ArrayProperty
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
