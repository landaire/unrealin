//! Engine bootstrap data for the unchecked decode.
//!
//! [`ENGINE_CLASS_WARMUP`] is the **retail NTSC** menu trace's
//! `object_load_order[..None.MyLevel]` (153 entries). Mirrors the
//! explicit `StaticLoadObject` calls in SC's `game_main`
//! (xbe `0x1F370`) before it triggers the level load via
//! `StaticLoadObject(MapName, ULevel, ...)`. Some entries are
//! loaded explicitly by `game_main`; the rest cascade in via
//! property-tag and class-ref deserialization as the explicit ones
//! go through `verify_imports`. We don't distinguish those cases —
//! `load_object_by_full_name` is idempotent (no-op on the second
//! call), so iterating the full list reproduces the exact engine
//! state regardless of which entries were "really" explicit.
//!
//! Retail SC NTSC ships at least 4 md5-distinct `common.lin` files,
//! but the warmup applies to every map regardless: the list mirrors
//! `game_main`'s explicit `StaticLoadObject` calls in the xbe (a
//! fixed code path independent of which `common.lin` variant the
//! map dir ships). The proto build mostly works with the same warmup
//! (verified empirically — coverage matches retail across 28 maps),
//! though the proto's `game_main` has its own load order; ours just
//! happens to overlap enough that the cascade reaches engine
//! quiescence regardless.
//!
//! **The DEMO build (Splinter Cell Demo, 2002-08-30) does NOT
//! work with this warmup.** Its common.lin is built against an
//! earlier engine snapshot with a different `game_main` static-
//! load order; replaying the retail warmup against demo data
//! over-loads the wrong classes, the cascade misaligns mid-Stage 1,
//! and read_package_header eventually asserts on a non-PKG_TAG.
//! Fixing this requires a QEMU-plugin trace recorded against demo
//! to derive its specific `object_load_order[..None.MyLevel]`.
//! Same path the retail warmup was derived from
//! (~/dev/UE2OffsetDump). Until then demo extraction is broken.

pub(crate) const ENGINE_CLASS_WARMUP: &[&str] = &[
    "Engine.GameEngine",
    "Echelon.ECanvas",
    "Engine.Input",
    "Core.Function",
    "Core.Struct",
    "Core.Field",
    "Core.Const",
    "Core.TextBuffer",
    "Core.Enum",
    "Core.LinkerLoad",
    "Core.Linker",
    "Core.LinkerSave",
    "Core.Commandlet",
    "Core.Factory",
    "Core.TextBufferFactory",
    "Core.Language",
    "Core.ByteProperty",
    "Core.Property",
    "Core.IntProperty",
    "Core.BoolProperty",
    "Core.FloatProperty",
    "Core.ObjectProperty",
    "Core.ClassProperty",
    "Core.NameProperty",
    "Core.StrProperty",
    "Core.FixedArrayProperty",
    "Core.ArrayProperty",
    "Core.MapProperty",
    "Core.StructProperty",
    "Core.System",
    "Core.Package",
    "Core.State",
    "Core.Class",
    "Engine.Light",
    "Engine.Keypoint",
    "Engine.ClipMarker",
    "Engine.PolyMarker",
    "Engine.Note",
    "Engine.Camera",
    "Engine.PathNode",
    "Engine.Scout",
    "Engine.InterpolationPoint",
    "Engine.Projectile",
    "Engine.LineOfSightTrigger",
    "Engine.Sound",
    "Engine.AudioSubsystem",
    "Engine.BeamEmitter",
    "Engine.Client",
    "Engine.Viewport",
    "Engine.RenderDevice",
    "Engine.ServerCommandlet",
    "Engine.GlobalTempObjects",
    "Engine.Polys",
    "Engine.Font",
    "Engine.Input",
    "Engine.Console",
    "Engine.LevelBase",
    "Engine.Level",
    "Engine.Primitive",
    "Engine.MeshInstance",
    "Engine.LodMeshInstance",
    "Engine.Mesh",
    "Engine.LodMesh",
    "Engine.ProxyBitmapMaterial",
    "Engine.TexCoordMaterial",
    "Engine.Modifier",
    "Engine.ColorModifier",
    "Engine.Shader",
    "Engine.Combiner",
    "Engine.TexModifier",
    "Engine.TexPanner",
    "Engine.TexScaler",
    "Engine.TexRotator",
    "Engine.TexOscillator",
    "Engine.TexEnvMap",
    "Engine.TexMatrix",
    "Engine.FinalBlend",
    "Engine.MeshEmitter",
    "Engine.Model",
    "Engine.ProjectorPrimitive",
    "Engine.RenderResource",
    "Engine.VertexStreamBase",
    "Engine.VertexStreamVECTOR",
    "Engine.VertexStreamCOLOR",
    "Engine.VertexStreamUV",
    "Engine.VertexStreamPosNormTex",
    "Engine.VertexBuffer",
    "Engine.IndexBuffer",
    "Engine.SkinVertexBuffer",
    "Engine.SceneManager",
    "Engine.ActionMoveCamera",
    "Engine.ActionPause",
    "Engine.SubActionFade",
    "Engine.SubActionTrigger",
    "Engine.SubActionFOV",
    "Engine.SubActionOrientation",
    "Engine.SubActionGameSpeed",
    "Engine.SubActionSceneSpeed",
    "Engine.LookTarget",
    "Engine.MeshEditProps",
    "Engine.AnimEditProps",
    "Engine.SequEditProps",
    "Engine.MeshAnimation",
    "Engine.Animation",
    "Engine.SkeletalMesh",
    "Engine.SkeletalMeshInstance",
    "Engine.SparkEmitter",
    "Engine.SpriteEmitter",
    "Engine.StaticMesh",
    "Engine.StaticMeshInstance",
    "Engine.StaticMeshActor",
    "Engine.TerrainSector",
    "Engine.TerrainPrimitive",
    "Engine.Cubemap",
    "Engine.VolumeTexture",
    "Engine.EchelonEnums",
    "Engine.EAIEvent",
    "Engine.CollisionMeshActor",
    "Engine.EGlow",
    "Engine.ESoftBodyActor",
    "Engine.ERopeActor",
    "Engine.ESoftBody",
    "Engine.ERope",
    "Engine.EOceanPrimitive",
    "Engine.ERainVolume",
    "Engine.ERainPrimitive",
    "Engine.AnimInfo",
    "Engine.ProjectorRenderInfo",
    "Engine.ConvexVolume",
    "Engine.AntiPortalActor",
    "Echelon.EchelonGameInfo",
    "Echelon.PatrolPoint",
    "Echelon.EDoorPoint",
    "Echelon.EDoorMarker",
    "Echelon.EBitTable",
    "Echelon.ESearchManager",
    "Echelon.EGamePlayObjectLight",
    "Echelon.ESoundVolume",
    "Echelon.EGObjectGroup",
    "EchelonIngredient.ESensor",
    "EchelonIngredient.EFlare",
    "EchelonIngredient.EWallMine",
    "EchelonIngredient.ESBPatchActor",
    "EchelonIngredient.ESBRopeActor",
    "EchelonIngredient.ESBChainActor",
    "EchelonIngredient.ESBStripDoorActor",
    "EchelonIngredient.ESBPatch",
    "EchelonIngredient.ESBRope",
    "EchelonIngredient.ESBChain",
    "EchelonIngredient.ESBStripDoor",
    "EchelonHUD.EMenuHUD",
    "EchelonHUD.EMainMenuHUD",
    "ESam.SamAMesh",
];

/// Engine class loads triggered AFTER `LoadMap`'s MyLevel cascade
/// completes. Derived from every retail trace's
/// `object_load_order[None.MyLevel..]` tail; each map's trace records
/// the same fixed-shape suffix (after per-map ENPC anim iteration):
///
/// ```text
/// EchelonPattern.VGame
/// Engine.InteractionMaster
/// Engine.Console
/// Echelon.EPlayerController
/// EchelonCharacter.ESam
/// Echelon.EGameInteraction
/// EchelonHUD.EchelonMainHUD
/// ```
///
/// In the engine these are scripted loads — `EchelonGameInfo`'s
/// `PreBeginPlay`/`InitGame`/`PostBeginPlay` chain, plus the
/// `PlayerController`/`Pawn` spawn that `LoadMap` does after the
/// level cascade (xbe `0x81860`, the block after `StaticLoadObject(
/// MyLevel, ...)` and the actor-iteration vtable calls). The exact
/// scripts vary by class, but the resulting `LoadObject` calls are
/// the fixed list above.
///
/// We can't simulate the full UScript VM, so we replay these as
/// explicit `load_object_by_full_name` calls after `run_post_cascade`.
/// Triggering the class umbrellas pulls in their function children
/// + CDO defaults, which transitively loads referenced `Sound`/
/// `MeshAnimation` imports via property-tag resolution — closing the
/// trailing-sounds gap that `EchelonCharacter.ESam`'s defaults
/// account for.
pub(crate) const POST_LEVEL_LOAD_LIST: &[&str] = &[
    "EchelonPattern.VGame",
    "Engine.InteractionMaster",
    "Engine.Console",
    "Echelon.EPlayerController",
    "EchelonCharacter.ESam",
    "Echelon.EGameInteraction",
    "EchelonHUD.EchelonMainHUD",
];
