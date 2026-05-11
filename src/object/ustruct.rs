use std::io;
use std::rc::Rc;

use byteorder::ReadBytesExt;
use tracing::Level;
use tracing::debug;
use tracing::span;

use crate::de::ExportIndex;
use crate::de::Linker;
use crate::de::RcLinker;
use crate::object::BodyKind;
use crate::object::DeserializeUnrealObject;
use crate::object::RcUnrealObject;
use crate::object::SerializeUnrealObject;
use crate::object::UObjectKind;
use crate::object::UnrealObject;
use crate::object::builtins::Property;
use crate::object::internal::script::Expr;
use crate::object::internal::script::{self};
use crate::object::link_object;
use crate::object::ufield::Field;
use crate::object::uproperty::PropertyFlags;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

#[derive(Default, Debug)]
pub struct Struct {
    pub parent_object: Field,

    script_text: Option<RcUnrealObject>,
    pub children: Option<RcUnrealObject>,

    friendly_name: i32,

    flags: u32,
    line: u32,
    text_pos: u32,
    pub script_size: u32,
    /// In-memory script bytecode parsed into a lossless `Expr` tree.
    /// Re-emit walks this via `script::serialize_expr` so the on-disk
    /// bytes don't carry the peek-back artifacts that contaminate the
    /// captured raw stream.
    pub parsed_script: Vec<Expr>,
    /// Verbatim source bytes consumed for the script section of this
    /// struct's body. Sliced out of the active capture frame around the
    /// script-bytecode reads. Used as the diagnostic ground-truth: a
    /// matching `serialize_expr(parsed_script)` confirms the parsed
    /// tree is lossless; mismatches surface peek-back artifacts or
    /// missing branches in `deserialize_expr`.
    pub script_capture: Vec<u8>,
    /// Byte offset within the export's captured body where the script
    /// section begins (immediately after the script_size u32). The
    /// `serialize` impl uses this to splice canonical
    /// `serialize_expr(parsed_script)` bytes back into the body,
    /// replacing the peek-back-contaminated raw stream while leaving
    /// the rest of the body verbatim.
    pub script_section_start: usize,
}

impl Struct {
    pub fn visit_children(&self, kind: UObjectKind) {
        let mut current_field = self.children.as_ref().map(Rc::clone);
        loop {
            // Try to grab the next field for this struct
            while let Some(field) = current_field.as_ref().map(Rc::clone) {
                let field_inner = field.borrow();
                if field_inner.is_a(kind) {
                    break;
                }

                let as_field = field_inner
                    .parent_of_kind(UObjectKind::Field)
                    .expect("failed to find parent of kind Field")
                    .as_any()
                    .downcast_ref::<Field>()
                    .expect("failed to cast field to Field");

                current_field = as_field.next();
            }

            let Some(child) = current_field else {
                break;
            };

            let span = span!(Level::DEBUG, "ustruct_property");
            let _enter = span.enter();

            let mut child_inner = child.borrow_mut();
            let child_any = child_inner
                .parent_of_kind_mut(kind)
                .expect("failed to resolve parent of requested kind")
                .as_any_mut();

            let child_as_property = child_any
                .downcast_mut::<Property>()
                .expect("failed to cast child as Property");

            if child_as_property.flags().contains(PropertyFlags::NET) {
                todo!("handle property");
            }

            let as_field = child_inner
                .parent_of_kind(UObjectKind::Field)
                .expect("failed to find parent of kind Field")
                .as_any()
                .downcast_ref::<Field>()
                .expect("failed to cast field to Field");

            current_field = as_field.next();
        }

        // Try to grab the super struct? Use try_borrow because the super may
        // currently be borrowed by an outer `deserialize` higher in the stack
        // (recursive super-preload re-entry through this same super chain).
        // The super's children will be walked when its own deserialize ends,
        // so skipping here is fine. This loop only checks `PropertyFlags::NET`.
        if let Some(super_field) = self.parent_object.super_field()
            && let Ok(super_inner) = super_field.try_borrow()
        {
            let super_struct = super_inner
                .parent_of_kind(UObjectKind::Struct)
                .expect("failed to get parent Struct")
                .as_any()
                .downcast_ref::<Struct>()
                .expect("failed to cast parent as Struct");

            super_struct.visit_children(kind);
        }
    }
}

impl SerializeUnrealObject for Struct {
    /// Replace the script-section bytes in the captured body with the
    /// canonical re-emit from `parsed_script`. The rest of the body
    /// (Object preamble, Field super/next, the four phantom fields,
    /// the `script_size` u32, and any subclass extras emitted after
    /// the script section) stays verbatim.
    ///
    /// Why splice instead of full reconstruct: the engine's load path
    /// peek-and-rewinds inside script bytecode, so the captured raw
    /// stream contains duplicate bytes that the engine would NOT emit
    /// on save. The parsed `Expr` tree is canonical (each DebugInfo
    /// shows up exactly once, etc.), so `serialize_expr` produces what
    /// UExplorer's bytecode decompiler expects. Everything outside the
    /// script section in the captured body is byte-identical to the
    /// engine's save output, so we leave that part alone.
    fn serialize<E>(
        &self,
        _linker: &Linker,
        _export_index: ExportIndex,
        captured: &[u8],
    ) -> std::io::Result<BodyKind>
    where
        E: byteorder::ByteOrder,
    {
        let start = self.script_section_start;
        let end = start + self.script_capture.len();
        if end > captured.len() {
            tracing::warn!(
                "Struct::serialize: script_section bounds {start}..{end} exceed body len {}; falling back to opaque",
                captured.len(),
            );
            return Ok(BodyKind::Opaque(captured.to_vec()));
        }

        let mut canonical = Vec::with_capacity(self.script_capture.len());
        script::serialize_expr::<_, E>(&self.parsed_script, &mut canonical)?;

        if canonical == self.script_capture {
            // Nothing to splice; the captured stream is already canonical.
            return Ok(BodyKind::Opaque(captured.to_vec()));
        }

        let mut body =
            Vec::with_capacity(captured.len() - self.script_capture.len() + canonical.len());
        body.extend_from_slice(&captured[..start]);
        body.extend_from_slice(&canonical);
        body.extend_from_slice(&captured[end..]);
        Ok(BodyKind::Reconstructed(body))
    }
}

impl DeserializeUnrealObject for Struct {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_struct");
        let _enter = span.enter();

        let licensee_version = linker.borrow().licensee_version();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        debug!("deserializing script_text");
        // SC's `UStruct::Serialize` (xbe `0x2b8c0`) reads ScriptText
        // through `operator<<<UObject*>(&local_var)` where `local_var`
        // is initialized to 0 -- i.e. on load it reads-and-discards, on
        // save it writes a null packed_int. Replicating that on the
        // capture side: we still consume the source bytes (cursor must
        // advance for the next field) but splice the captured frame so
        // the re-emitted body holds a single 0x00 there. UExplorer was
        // following the LIN's stale non-null reference into a
        // `LOAD_FOR_EDIT`-only ScriptText export whose body is empty
        // and throwing NullReferenceException.
        let pre = reader.capture_len();
        self.script_text = reader.read_object::<E>(runtime, linker)?;
        let consumed = reader.capture_len() - pre;
        reader.splice_capture_tail(consumed, &[0]);

        debug!("deserializing children");
        self.children = reader.read_object::<E>(runtime, linker)?;

        // FriendlyName, Line, TextPos: SC's `UStruct::Serialize` reads
        // them; the engine's save path writes zeros for all three. UE2
        // tools split on FriendlyName: UELib's export-table load uses
        // `ObjectName` (correct), but UE-Explorer's decompiler reads the
        // in-bytecode FriendlyName from this position (UStruct.cs:255 in
        // Unreal-Library), so a spliced zero renders every function as
        // `event None(...)` in decompiled output. We preserve the
        // captured original here so decompilers see the real name.
        // Line and TextPos still get spliced because they're not visible
        // in decompile and zero-splicing keeps closer parity with the
        // engine save output.
        debug!("deserializing friendly_name");
        self.friendly_name = reader.read_packed_int()?;

        if licensee_version > 0x1A && runtime.game != crate::de::Game::PandoraTomorrow {
            self.flags = reader.read_u32::<E>()?;
        }

        debug!("deserializing line");
        let pre = reader.capture_len();
        self.line = reader.read_u32::<E>()?;
        let consumed = reader.capture_len() - pre;
        reader.splice_capture_tail(consumed, &[0; 4]);

        debug!("deserializing text_pos");
        let pre = reader.capture_len();
        self.text_pos = reader.read_u32::<E>()?;
        let consumed = reader.capture_len() - pre;
        reader.splice_capture_tail(consumed, &[0; 4]);

        debug!("deserializing script_size");
        self.script_size = reader.read_u32::<E>()?;

        let start_pos = reader.stream_position()?;
        let expected_end_pos = start_pos + self.script_size as u64;
        debug!(
            "deserializing script. start_pos= {start_pos:#X}, expected_end= {expected_end_pos:#X}, len= {:#X}",
            self.script_size
        );

        let mut bytes_read = 0;
        let mut parsed = Vec::new();
        let script_capture_start = reader.capture_len();
        self.script_section_start = script_capture_start;

        while bytes_read < self.script_size as usize {
            debug!("Bytes read: {bytes_read:#X} / {:#X}", self.script_size);
            parsed.append(&mut script::deserialize_expr::<E, _>(
                runtime,
                linker,
                reader,
                &mut bytes_read,
                self.script_size as usize,
            )?);
        }

        if bytes_read != self.script_size as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "script body under/over-read: got {} bytes, expected {} \
                     (cursor likely mis-aligned with engine read order)",
                    bytes_read, self.script_size
                ),
            ));
        }

        self.parsed_script = parsed;
        self.script_capture = reader.capture_slice_from(script_capture_start);

        // Deserialize properties. UStruct::Link
        //
        // First, ensure that the super field is fully loaded
        if let Some(super_field) = self.parent_object.super_field() {
            debug!(
                "Full loading super field for {}",
                self.base_object().concrete_object_kind().as_str()
            );
            runtime.full_load_object::<E, _>(&super_field, reader)?;
        }

        let mut child_ptr = self.children.clone();
        while let Some(child) = child_ptr {
            let span = span!(
                Level::DEBUG,
                "ustruct_link_child_iter",
                child_ptr = format!("{:#x}", child.as_ptr().expose_provenance())
            );
            let _enter = span.enter();

            debug!("Full loading child object");
            runtime.full_load_object::<E, _>(&child, reader)?;

            // Do not do any more work if the field is nto related to this struct
            let child_inner = child.borrow();
            let Some(field_outer) = child_inner.base_object().outer_object() else {
                break;
            };

            {
                let this_concrete = self.base_object().concrete_obj();
                if !Rc::ptr_eq(field_outer, &this_concrete) {
                    break;
                }
            }

            // Link the struct
            if child_inner.is_a(UObjectKind::Property) {
                let child_linker = child.borrow().base_object().linker();

                link_object::<E, _>(runtime, Rc::clone(&child), &child_linker, reader)?;
            }

            let child_inner = child.borrow();

            let parent_field = child_inner
                .parent_of_kind(UObjectKind::Field)
                .expect("could not get parent Field");

            child_ptr = parent_field
                .as_any()
                .downcast_ref::<Field>()
                .expect("failed to cast parent field to Field")
                .next();
        }

        // Handle properties with flags. This needs to walk up from the current struct,
        // through its fields, then to the next inheritence struct
        self.visit_children(UObjectKind::Property);

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::UObjectKind;
    use crate::object::UnrealObject;
    use crate::object::test_common::test_object_is_a;

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Struct]
            .iter()
            .cloned()
            .chain(crate::object::ufield::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = Struct::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
