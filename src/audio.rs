//! Xbox-ADPCM (XbA) decoder, format `wFormatTag = 0x0069`.
//!
//! Identified from `splintercell.xbe` static analysis: `sub_1885a0`
//! sets up a `WAVEFORMATEX` with `wFormatTag = 0x69`,
//! `wBitsPerSample = 4`, `cbSize = 2`, and `nBlockAlign = nChannels *
//! 36`, then calls `IDirectSound::CreateSoundBuffer` and hands the
//! audio bytes to the MCPX hardware decoder via `sub_18cb60`
//! (`IDirectSoundBuffer::SetBufferData`). A QEMU plugin trace
//! capture confirmed the `SetBufferData` payload is identical to the
//! corresponding bytes in the source `.SM2` file -- no software
//! re-encoding step.
//!
//! The earlier "DARE-IMA-ADPCM v3" port in this file targeted the
//! PC-build's CPU decoder (`sub_1942e0` / `sub_1946d0`) -- code that
//! is present in `splintercell.xbe` but not executed at runtime on
//! Xbox. That code is gone; the math here uses the standard MS
//! IMA-ADPCM step-size table, which is what XbA shares with all
//! other IMA-ADPCM variants.
//!
//! Block layout per channel:
//!   - 2 bytes: predictor (i16 LE) -- initial sample for the block
//!   - 1 byte:  step_index (0..88)
//!   - 1 byte:  reserved (always 0)
//!   - 32 bytes: 64 nibbles, packed low-nibble-first
//!
//! Stereo blocks are 72 bytes laid out as: 4-byte header_L,
//! 4-byte header_R, then 4-byte data chunks alternating L/R for
//! eight rounds (4*16 = 64 bytes of data).

/// Per-channel block size in bytes (4-byte header + 32 data bytes).
pub const BLOCK_BYTES_PER_CHANNEL: usize = 36;

/// Samples decoded per block per channel (32 bytes * 2 nibbles/byte).
pub const SAMPLES_PER_BLOCK: usize = 64;

/// Standard MS IMA-ADPCM step-size table (89 entries). Used by every
/// IMA-ADPCM variant including XbA -- the table has been frozen since
/// the original IMA spec.
const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// Standard MS IMA-ADPCM step-index adjustment table. Indexed by the
/// 4-bit nibble. Note that the magnitude bits (0..3) of the nibble
/// share the same adjustment for the sign-positive (0..7) and
/// sign-negative (8..15) halves.
const INDEX_TABLE: [i8; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

#[derive(Copy, Clone, Debug)]
struct ChannelState {
    predictor: i32,
    step_index: i32,
}

impl ChannelState {
    fn from_header(header: [u8; 4]) -> Self {
        let predictor = i16::from_le_bytes([header[0], header[1]]) as i32;
        let step_index = (header[2] as i32).clamp(0, 88);
        Self {
            predictor,
            step_index,
        }
    }

    /// Decode one 4-bit nibble into a sample, advancing predictor and
    /// step_index. Uses the canonical multiply-then-shift form
    /// `diff = ((2*magnitude + 1) * step) >> 3`. Bit-by-bit shifted
    /// accumulation (`(step>>3) + (step>>2)*bit0 + (step>>1)*bit1 +
    /// step*bit2`) is mathematically equivalent in real arithmetic
    /// but loses precision per term under integer truncation when
    /// `step` is small -- for `step=7`, bit-form gives `1` and the
    /// canonical form gives `2`. ffmpeg's `adpcm_ima_expand_nibble`
    /// uses the canonical form; matching it makes the output bit-for-
    /// bit identical to ffmpeg's reference XbA decoder.
    fn decode_nibble(&mut self, nibble: u8) -> i16 {
        let step = STEP_TABLE[self.step_index as usize];
        let magnitude = (nibble & 7) as i32;
        let diff = ((2 * magnitude + 1) * step) >> 3;
        if nibble & 8 != 0 {
            self.predictor -= diff;
        } else {
            self.predictor += diff;
        }
        self.predictor = self.predictor.clamp(-32768, 32767);
        self.step_index = (self.step_index + INDEX_TABLE[nibble as usize] as i32).clamp(0, 88);
        self.predictor as i16
    }
}

/// Decode a single mono XbA block. `block` must be exactly 36 bytes.
/// Output is 64 samples -- the predictor is the seed for decoding the
/// 64 nibbles, not emitted as a sample. (`nSamplesPerBlock` from the
/// engine's WAVEFORMATEX is 64, confirming the predictor is not part
/// of the output count.)
fn decode_block_mono(block: &[u8; BLOCK_BYTES_PER_CHANNEL]) -> [i16; SAMPLES_PER_BLOCK] {
    let mut state = ChannelState::from_header([block[0], block[1], block[2], block[3]]);
    let mut out = [0i16; SAMPLES_PER_BLOCK];
    for (i, &byte) in block[4..].iter().enumerate() {
        out[i * 2] = state.decode_nibble(byte & 0x0f);
        out[i * 2 + 1] = state.decode_nibble((byte >> 4) & 0x0f);
    }
    out
}

/// Decode mono XbA. `data.len()` must be a multiple of 36.
pub fn decode_mono(data: &[u8]) -> Vec<i16> {
    assert!(
        data.len().is_multiple_of(BLOCK_BYTES_PER_CHANNEL),
        "mono XbA stream length {} is not a multiple of block size {}",
        data.len(),
        BLOCK_BYTES_PER_CHANNEL,
    );
    let block_count = data.len() / BLOCK_BYTES_PER_CHANNEL;
    let mut out = Vec::with_capacity(block_count * SAMPLES_PER_BLOCK);
    for chunk in data.chunks_exact(BLOCK_BYTES_PER_CHANNEL) {
        let block: &[u8; BLOCK_BYTES_PER_CHANNEL] = chunk.try_into().unwrap();
        out.extend_from_slice(&decode_block_mono(block));
    }
    out
}

/// Decode stereo XbA. `data.len()` must be a multiple of 72.
///
/// Stereo block layout (72 bytes total):
///   - bytes  0..3:  header_L (predictor + step + reserved)
///   - bytes  4..7:  header_R
///   - bytes  8..11: 4 data bytes for L (8 nibbles)
///   - bytes 12..15: 4 data bytes for R (8 nibbles)
///   - bytes 16..19: 4 data bytes for L
///   - bytes 20..23: 4 data bytes for R
///   - ... eight L/R rounds total ...
///   - bytes 64..67: 4 data bytes for L (final round)
///   - bytes 68..71: 4 data bytes for R
///
/// Output is interleaved L/R i16 samples.
pub fn decode_stereo(data: &[u8]) -> Vec<i16> {
    const STEREO_BLOCK: usize = BLOCK_BYTES_PER_CHANNEL * 2;
    assert!(
        data.len().is_multiple_of(STEREO_BLOCK),
        "stereo XbA stream length {} is not a multiple of {}",
        data.len(),
        STEREO_BLOCK,
    );
    let block_count = data.len() / STEREO_BLOCK;
    let mut out = Vec::with_capacity(block_count * SAMPLES_PER_BLOCK * 2);
    for chunk in data.chunks_exact(STEREO_BLOCK) {
        let mut left = ChannelState::from_header([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let mut right = ChannelState::from_header([chunk[4], chunk[5], chunk[6], chunk[7]]);
        // 8 rounds x 4 data bytes per channel x 2 nibbles per byte = 64 samples per channel.
        let data_section = &chunk[8..];
        let mut l_samples = [0i16; SAMPLES_PER_BLOCK];
        let mut r_samples = [0i16; SAMPLES_PER_BLOCK];
        for round in 0..8 {
            let l_off = round * 8;
            let r_off = l_off + 4;
            for i in 0..4 {
                let l_byte = data_section[l_off + i];
                let r_byte = data_section[r_off + i];
                let sample_idx = round * 8 + i * 2;
                l_samples[sample_idx] = left.decode_nibble(l_byte & 0x0f);
                l_samples[sample_idx + 1] = left.decode_nibble((l_byte >> 4) & 0x0f);
                r_samples[sample_idx] = right.decode_nibble(r_byte & 0x0f);
                r_samples[sample_idx + 1] = right.decode_nibble((r_byte >> 4) & 0x0f);
            }
        }
        for i in 0..SAMPLES_PER_BLOCK {
            out.push(l_samples[i]);
            out.push(r_samples[i]);
        }
    }
    out
}

/// Generic dispatch: mono if `channels == 1`, stereo if `channels == 2`.
pub fn decode(data: &[u8], channels: u16) -> Vec<i16> {
    match channels {
        1 => decode_mono(data),
        2 => decode_stereo(data),
        n => panic!("unsupported channel count: {n} (expected 1 or 2)"),
    }
}

/// Shared audio-domain newtypes. Codec modules and CLI all consume
/// these to keep raw u32/u8 from drifting into ambiguous roles.
pub mod units {
    /// Sample rate in Hz.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct SampleRate(u32);

    impl SampleRate {
        pub const fn new(hz: u32) -> Self {
            Self(hz)
        }
        pub const fn hz(self) -> u32 {
            self.0
        }
    }

    impl std::fmt::Display for SampleRate {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{} Hz", self.0)
        }
    }

    /// Engine-assigned identifier for a per-map sound entry in SM2/LM2.
    /// Game scripts reference sounds by `SeqId`, not by table position;
    /// inserting a new sound should pick a fresh `SeqId`, not shift
    /// existing ones.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
    pub struct SeqId(u32);

    impl SeqId {
        pub const fn new(v: u32) -> Self {
            Self(v)
        }
        pub const fn raw(self) -> u32 {
            self.0
        }
    }

    impl std::fmt::LowerHex for SeqId {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::LowerHex::fmt(&self.0, f)
        }
    }

    /// 16:16 fixed-point playback-rate ratio from SM2/LM2 `array_a`.
    /// `RateRatio::UNIT == 1.0`; the engine multiplies the nominal
    /// rate by `ratio / UNIT.0` to get the effective playback rate.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct RateRatio(u32);

    impl RateRatio {
        pub const UNIT: Self = Self(0x10000);

        pub const fn new(raw: u32) -> Self {
            Self(raw)
        }
        pub const fn raw(self) -> u32 {
            self.0
        }
        pub const fn is_zero(self) -> bool {
            self.0 == 0
        }

        /// Scale `rate` by this ratio. Done in u64 to avoid overflow
        /// for ratios above unity.
        pub fn apply(self, rate: SampleRate) -> SampleRate {
            SampleRate::new(((rate.hz() as u64 * self.0 as u64) / Self::UNIT.0 as u64) as u32)
        }
    }

    /// A 4-bit nibble, bounded `0..=15`. Wrapping construction
    /// guarantees the value is in range so kernel inputs don't need
    /// to re-mask at every call site.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct Nibble(u8);

    impl Nibble {
        pub const fn new(v: u8) -> Self {
            Self(v & 0x0F)
        }
        pub const fn lo(byte: u8) -> Self {
            Self(byte & 0x0F)
        }
        pub const fn hi(byte: u8) -> Self {
            Self((byte >> 4) & 0x0F)
        }
        pub const fn value(self) -> u8 {
            self.0
        }
    }
}

/// Codec id 3 -- the streamed-audio variant used by `.SS2`/`.LS2`
/// files. Distinct from the SM2/LM2 hardware-decoded XbA above:
/// codec 3 streams are CPU-decoded by the engine, going through the
/// `sub_193950` Decode entry which calls `sub_193780` (InitHeader)
/// once and `sub_193510` per refill. The per-block kernel is
/// `sub_198600`, a thin dispatcher that routes by the mode flag in
/// the header (file byte 12, latched into codec state at `+0x54`):
///
///   - `mode == 0` -> `sub_1983a0`: one channel at a time (mono or
///     planar stereo).
///   - `mode == 1` -> `sub_1984a0`: L/R nibbles interleaved within
///     each byte (LOW = L, HIGH = R), output written interleaved.
///
/// Both kernels run on STANDARD IMA-ADPCM math:
///   `delta = ((nibble & 7) * 2 + 1) * step >> 3`
///   `predictor += (nibble & 8) ? -delta : +delta`
///   `predictor.clamp(-32768, 32767)`
///   `step_index += INDEX_TABLE[nibble & 0xf]`
///   `step_index.clamp(0, 88)`
///
/// confirmed by reading the `data_2d05c0` (16-entry index table) and
/// `data_2d0600` (89-entry step table) data sections -- they're
/// byte-for-byte identical to `INDEX_TABLE` and `STEP_TABLE` above.
/// The codec ships the tables inline rather than reusing a single
/// copy, but the values match.
///
/// We expose only the kernels here; the per-stream state machine
/// (`sub_193950`'s pad-sample handling for output-count parity) is
/// for incremental Decode-call boundaries and isn't needed when
/// decoding a stream end-to-end.
pub mod codec3 {
    use super::INDEX_TABLE;
    use super::STEP_TABLE;

    /// Per-channel running state. `step_index` is stored as `i32` for
    /// arithmetic convenience; on disk and in the engine's struct
    /// it's a single signed byte.
    #[derive(Copy, Clone, Debug)]
    pub struct ChannelState {
        pub predictor: i32,
        pub step_index: i32,
    }

    impl ChannelState {
        pub fn new(predictor: i16, step_index: u8) -> Self {
            Self {
                predictor: predictor as i32,
                step_index: (step_index as i32).clamp(0, 88),
            }
        }
    }

    /// Decode one nibble. Mirrors the inner of `sub_1983a0` /
    /// `sub_1984a0` exactly -- same pre-step lookup, same clamp
    /// constants. Updates the channel state in place and returns
    /// the produced i16 sample.
    #[inline]
    fn step(state: &mut ChannelState, nibble: u8) -> i16 {
        let step_size = STEP_TABLE[state.step_index as usize];
        let magnitude = (nibble & 7) as i32;
        let delta = ((2 * magnitude + 1) * step_size) >> 3;
        if nibble & 8 != 0 {
            state.predictor -= delta;
        } else {
            state.predictor += delta;
        }
        state.predictor = state.predictor.clamp(-32768, 32767);
        state.step_index =
            (state.step_index + INDEX_TABLE[(nibble & 0xf) as usize] as i32).clamp(0, 88);
        state.predictor as i16
    }

    /// Decode `count` samples for one channel from `data`. Each input
    /// byte yields two samples -- DARE's IMA-ADPCM variant emits the
    /// HIGH nibble first, then the LOW nibble (the engine's
    /// `sub_1983a0` body does `eax = byte >> 4` on the byte-load
    /// branch and `eax = stored_byte & 0xf` on the no-load branch).
    /// Standard MS IMA-ADPCM is the opposite order; we tested LOW-
    /// first and got a metallic "tin can" / "breathing" artifact on
    /// transients while speech remained recognizable, which is the
    /// signature of nibble-order swap.
    ///
    /// `data.len()` must be at least `count.div_ceil(2)`. `state`
    /// is updated; pass the same instance back to continue the
    /// stream. Returns a `Vec<i16>` of exactly `count` samples.
    pub fn decode_planar(data: &[u8], state: &mut ChannelState, count: usize) -> Vec<i16> {
        let bytes_needed = count.div_ceil(2);
        assert!(
            data.len() >= bytes_needed,
            "decode_planar: need {bytes_needed} bytes for {count} samples, got {}",
            data.len(),
        );
        let mut out = Vec::with_capacity(count);
        let mut bytes = data.iter().copied();
        let mut current_byte: u8 = 0;
        for i in 0..count {
            let nibble = if i & 1 == 0 {
                current_byte = bytes.next().unwrap();
                (current_byte >> 4) & 0x0f
            } else {
                current_byte & 0x0f
            };
            out.push(step(state, nibble));
        }
        out
    }

    /// Decode `frame_count` interleaved L/R stereo frames from
    /// `data`. Each input byte holds one L sample (HIGH nibble)
    /// and one R sample (LOW nibble) -- DARE's `sub_1984a0`
    /// processes byte's high nibble first (toggle `ebp_1`
    /// initialized to 1, the byte-load branch is taken first and
    /// shifts right by 4), then the low nibble on the next
    /// iteration. Output is interleaved L,R,L,R,... so length is
    /// `frame_count * 2` i16 samples.
    pub fn decode_interleaved_stereo(
        data: &[u8],
        state: &mut [ChannelState; 2],
        frame_count: usize,
    ) -> Vec<i16> {
        assert!(
            data.len() >= frame_count,
            "decode_interleaved_stereo: need {frame_count} bytes, got {}",
            data.len(),
        );
        let mut out = Vec::with_capacity(frame_count * 2);
        for &byte in &data[..frame_count] {
            let l_nibble = (byte >> 4) & 0x0f;
            let r_nibble = byte & 0x0f;
            out.push(step(&mut state[0], l_nibble));
            out.push(step(&mut state[1], r_nibble));
        }
        out
    }

    /// Codec id 3 file header. Parsed by `sub_193780` from the
    /// first 28 bytes of the file via the stream's vtable[0x34]
    /// reader, then unpacked into the codec object's state struct
    /// at `+0x58..+0x73`. The byte-swap dance in the engine treats
    /// some fields as big-endian u16 -- we interpret them here as
    /// they reach the decoder, not as raw file bytes.
    ///
    /// Layout (file offsets):
    ///   `+0x00`  u8   version          -- must be 3.
    ///   `+0x01..+0x03`  3 bytes        -- sample-count high bits.
    ///   `+0x04..+0x07`  f32 LE         -- frame-period or scaling
    ///                                    factor (≈ 0.06 in observed
    ///                                    files).
    ///   `+0x08..+0x0B`  u32 LE         -- `track_count`: number of
    ///                                    SEQUENTIAL ADPCM clips
    ///                                    packed into the file.
    ///                                    The engine plays them in
    ///                                    order as the mission
    ///                                    progresses -- they are not
    ///                                    stereo channels nor
    ///                                    independent variants but
    ///                                    consecutive dialogue
    ///                                    fragments of one logical
    ///                                    track. Confirmed on
    ///                                    `0_0_2.LS2` (track_count=2):
    ///                                    track 0 is the first part
    ///                                    of Lambert's tutorial, and
    ///                                    track 1 picks up where it
    ///                                    leaves off. Examples
    ///                                    observed on disc:
    ///                                       `0_0.LS2`        = 1
    ///                                       `0_0_2.LS2`      = 2
    ///                                       `1_1_0.LS2`      = 3
    ///                                       `4_2.LS2`        = 7
    ///                                       `5_1.LS2`        = 7
    ///                                       `Music_Common.LS2` = 3
    ///                                    Each track's ADPCM stream
    ///                                    walks from (predictor=0,
    ///                                    step_index=0) -- they are
    ///                                    independent, just played
    ///                                    back-to-back.
    ///   `+0x0C`  u8                    -- mode flag latched into
    ///                                    `+0x54`. 0 = planar /
    ///                                    `sub_1983a0` per stream,
    ///                                    1 = interleaved-stereo /
    ///                                    `sub_1984a0` (true stereo,
    ///                                    used for music tracks).
    ///   `+0x0E..+0x0F`  big-endian u16 -- `+0x40` field, used by
    ///                                    `sub_193510` as a
    ///                                    block-availability counter
    ///                                    (decremented on each
    ///                                    inner-buffer fill).
    /// Decoder mode at file byte 12. `0` routes per-byte through
    /// `sub_1983a0` (one mono stream, two samples per byte); `1`
    /// routes through `sub_1984a0` (interleaved stereo, HIGH nibble
    /// = L, LOW nibble = R). All other values are rejected by the
    /// engine and by `parse_header`.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Mode {
        PlanarMono,
        InterleavedStereo,
    }

    impl Mode {
        pub const fn from_u8(v: u8) -> Option<Self> {
            match v {
                0 => Some(Self::PlanarMono),
                1 => Some(Self::InterleavedStereo),
                _ => None,
            }
        }
        pub const fn channels(self) -> u16 {
            match self {
                Self::PlanarMono => 1,
                Self::InterleavedStereo => 2,
            }
        }
        pub const fn as_u8(self) -> u8 {
            match self {
                Self::PlanarMono => 0,
                Self::InterleavedStereo => 1,
            }
        }
    }

    impl std::fmt::Display for Mode {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.as_u8())
        }
    }

    #[derive(Clone, Debug)]
    pub struct Header {
        pub version: u8,
        /// Count of sequential ADPCM clips concatenated in the
        /// file (NOT stereo channels). See struct doc.
        pub track_count: u32,
        pub mode: Mode,
        /// f32 at file offset `+0x04`. Empirically maps to sample
        /// rate via the `header_sample_rate()` helper.
        pub block_period: f32,
        /// Per-channel initial ADPCM state. The engine's
        /// `sub_193780` byte-swaps the u16 fields at file
        /// offsets `+0x10` (L predictor) and `+0x14` (R predictor)
        /// from BE -> LE on load, then `sub_193510` copies them
        /// into the live state at every block-refill. The kernel
        /// expects each block to begin with these seeds, not
        /// `(0, 0)`. Field layout in the file:
        ///   `+0x10..+0x11` BE u16 -> L predictor (signed)
        ///   `+0x12` u8           -> L step_index
        ///   `+0x14..+0x15` BE u16 -> R predictor (signed)
        ///   `+0x16` u8           -> R step_index
        pub init_l: ChannelState,
        pub init_r: ChannelState,
    }

    /// Bytes the engine reads as the codec_id=3 header (matches
    /// `sub_193780`'s `var_10c = 0x1c`).
    pub const HEADER_BYTES: usize = 28;

    /// File offset where ADPCM data begins. The engine reads
    /// exactly 28 bytes (`HEADER_BYTES`) for the header in
    /// `sub_193780`, then `sub_193510` reads the rest of the
    /// file sequentially and feeds it to `sub_198600` -- i.e.
    /// the ADPCM stream begins immediately at `+0x1C`.
    ///
    /// In `0_0_2.LS2` (dialogue, mode=0) bytes `+0x1C..+0x47`
    /// happen to be zero, so the kernel state evolves trivially
    /// across them; an earlier draft used `DATA_OFFSET = 0x48`
    /// and got away with it for that file. But STREAM.SS2 main
    /// sub-streams have non-zero data starting at `+0x20`
    /// (post-header) which evolves the predictor significantly
    /// before reaching `+0x48`. Skipping those bytes from
    /// `(0, 0, 0, 0)` initial state caused unrecoverable state
    /// divergence and produced pure noise output until this was
    /// corrected to `+0x1C`.
    pub const DATA_OFFSET: usize = HEADER_BYTES;

    pub fn parse_header(file_bytes: &[u8]) -> Result<Header, &'static str> {
        if file_bytes.len() < HEADER_BYTES {
            return Err("file too short for codec_id=3 header");
        }
        if file_bytes[0] != 3 {
            return Err("not a codec_id=3 file (first byte != 0x03)");
        }
        let track_count =
            u32::from_le_bytes([file_bytes[8], file_bytes[9], file_bytes[10], file_bytes[11]]);
        // track_count = 0 is observed on most files (dialogue
        // and the Music_*.SS2 stereo set). The handful with
        // non-zero values (`0_0=1, 0_0_2=2, 1_1_0=3,
        // Music_Common.LS2=3, 4_2=5_1=7`) all use it as
        // game-script metadata -- not byte structure.
        // Sanity bound; observed range is 0..=17 across retail
        // standalone files and STREAM.SS2 sub-streams. 256 is a
        // soft upper limit to reject obvious garbage headers.
        if track_count > 256 {
            return Err("track_count out of range");
        }
        let mode = Mode::from_u8(file_bytes[12]).ok_or("unsupported mode (must be 0 or 1)")?;
        let block_period =
            f32::from_le_bytes([file_bytes[4], file_bytes[5], file_bytes[6], file_bytes[7]]);
        // Initial ADPCM state. A QEMU plugin trace hooking
        // `sub_198600` (the per-block kernel) showed the engine
        // enters every block with state seeded to (0, 0, 0, 0)
        // -- NOT the BE u16 of file +0x10..+0x17 that an earlier
        // version of this code extracted. The bytes at
        // +0x10..+0x17 ARE non-zero in some files (e.g.
        // Music_Common.SS2 has `ff cb 1d 00 ff ce 1a 02`) but
        // they're per-stream metadata, not predictor seeds.
        // State evolution across blocks is continuous; the 21%
        // of trace captures with state == (0, 0, 0, 0) line up
        // with natural quiet passages where step_index decays to
        // 0 and predictor returns near zero, not periodic resets.
        let init_l = ChannelState {
            predictor: 0,
            step_index: 0,
        };
        let init_r = ChannelState {
            predictor: 0,
            step_index: 0,
        };
        Ok(Header {
            version: 3,
            track_count,
            mode,
            block_period,
            init_l,
            init_r,
        })
    }

    /// codec_id=3 streams have no per-file rate field. `sub_182b50`
    /// hardcodes 36000 Hz into the audio-system `WAVEFORMATEX`; the
    /// `block_period` f32 at `+0x04` and the BE u16 at `+0x0E..+0x0F`
    /// vary per file but neither encodes a rate (the latter is the
    /// remaining-bytes counter that `sub_193510` decrements per
    /// refill).
    pub fn header_sample_rate(_header: &Header) -> u32 {
        36000
    }

    /// Decode a complete codec_id=3 file. Returns mono i16 PCM
    /// for `mode=0` (the dominant case -- dialogue and most LS2
    /// content) or interleaved L/R i16 PCM for `mode=1` (true
    /// stereo, used by music).
    ///
    /// `mode=0` files contain ONE continuous IMA-ADPCM mono stream
    /// from `DATA_OFFSET` to end-of-file. Despite the
    /// `track_count` header field implying multiple sub-streams
    /// with state resets, decoding the whole payload as one mono
    /// stream produces output that is *byte-identical* to the
    /// split-then-concat alternative on `0_0_2.LS2` (`track_count
    /// = 2`, payload divisible). That confirms the encoder drives
    /// ADPCM state to (0,0) at clip boundaries -- either by
    /// design or because dialogue clips end in silence -- so a
    /// single continuous decode is equivalent to per-track
    /// resets. `track_count` is therefore metadata (script cue
    /// counts, streaming-subsystem chunking) rather than a
    /// byte-layout signal.
    pub fn decode_file(file_bytes: &[u8]) -> Result<Vec<i16>, &'static str> {
        let header = parse_header(file_bytes)?;
        if file_bytes.len() < DATA_OFFSET {
            return Err("file too short for ADPCM payload");
        }
        let data = &file_bytes[DATA_OFFSET..];

        match header.mode {
            Mode::PlanarMono => {
                let mut state = header.init_l;
                Ok(decode_planar(data, &mut state, data.len() * 2))
            }
            Mode::InterleavedStereo => {
                let mut state = [header.init_l, header.init_r];
                Ok(decode_interleaved_stereo(data, &mut state, data.len()))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Empty-input invariant: `count = 0` is well-defined and
        /// returns an empty vec without touching state.
        #[test]
        fn planar_zero_count_no_op() {
            let mut state = ChannelState::new(123, 5);
            let out = decode_planar(&[], &mut state, 0);
            assert!(out.is_empty());
            assert_eq!(state.predictor, 123);
            assert_eq!(state.step_index, 5);
        }

        /// Planar nibble[0]=0 with a non-zero predictor: at
        /// step_index=0 the step is 7, delta = (1*7) >> 3 = 0, so
        /// the predictor stays put on the first step.
        #[test]
        fn planar_first_zero_nibble_preserves_predictor() {
            let mut state = ChannelState::new(1234, 0);
            let out = decode_planar(&[0x00], &mut state, 1);
            assert_eq!(out[0], 1234);
        }

        /// Planar matches the engine kernel byte-for-byte on a max-
        /// positive sweep: 32 bytes of 0x77 (both nibbles = 7) walk
        /// the predictor up and saturate at +32767 by the end.
        #[test]
        fn planar_max_positive_saturates() {
            let mut state = ChannelState::new(0, 0);
            let out = decode_planar(&[0x77; 32], &mut state, 64);
            assert_eq!(*out.last().unwrap(), 32767);
        }

        /// Interleaved-stereo invariant: per-byte the HIGH nibble
        /// is L (DARE convention), LOW nibble is R. Byte 0x07 means
        /// L=0 (predictor unchanged), R=7 (max positive jump from
        /// step=7).
        #[test]
        fn stereo_byte_0x07_high_nibble_zero_low_nibble_seven() {
            let mut state = [ChannelState::new(100, 0), ChannelState::new(-200, 0)];
            let out = decode_interleaved_stereo(&[0x07], &mut state, 1);
            assert_eq!(out.len(), 2);
            // L: nibble=0, step=7, delta=0 -> predictor stays 100.
            assert_eq!(out[0], 100);
            // R: nibble=7, step=7, delta=((15)*7)>>3 = 13, predictor -200+13 = -187.
            assert_eq!(out[1], -187);
        }

        /// Stereo state-independence: when both channels receive the
        /// same nibble sequence (byte 0x77 -> L=7, R=7) their states
        /// stay locked together. Asymmetric input (e.g. 0x07) would
        /// diverge them; this test pins the locked-step case so a
        /// regression that crosses-bleeds state between channels
        /// breaks here.
        #[test]
        fn stereo_channels_are_independent() {
            let mut state = [ChannelState::new(0, 0), ChannelState::new(0, 0)];
            let _ = decode_interleaved_stereo(&[0x77, 0x77], &mut state, 2);
            assert_eq!(state[0].predictor, state[1].predictor);
            assert_eq!(state[0].step_index, state[1].step_index);
            // Two iterations of nibble=7 from (predictor=0, step_index=0):
            //   step 1: step=7,  delta=(15*7)>>3=13, predictor=13, step_index=8
            //   step 2: step=16, delta=(15*16)>>3=30, predictor=43, step_index=16
            assert_eq!(state[0].predictor, 43);
            assert_eq!(state[0].step_index, 16);
        }

        /// Asymmetric byte 0x70 (L = high nibble = 7, R = low nibble
        /// = 0) confirms channels DO diverge when fed different
        /// nibbles -- the symmetric test above isn't masking a
        /// state-leak bug.
        #[test]
        fn stereo_channels_diverge_on_asymmetric_input() {
            let mut state = [ChannelState::new(0, 0), ChannelState::new(0, 0)];
            let _ = decode_interleaved_stereo(&[0x70, 0x70], &mut state, 2);
            // L saw nibble 7 twice -> predictor=43, step_index=16.
            assert_eq!(state[0].predictor, 43);
            // R saw nibble 0 twice -> predictor stays 0, step_index
            // tries to go negative each step but clamps to 0.
            assert_eq!(state[1].predictor, 0);
            assert_eq!(state[1].step_index, 0);
        }

        fn valid_header_bytes(mode: u8) -> Vec<u8> {
            let mut bytes = vec![0u8; HEADER_BYTES];
            bytes[0] = 3;
            bytes[12] = mode;
            bytes
        }

        #[test]
        fn parse_rejects_short_file() {
            assert!(parse_header(&[0u8; HEADER_BYTES - 1]).is_err());
            assert!(parse_header(&[]).is_err());
        }

        #[test]
        fn parse_rejects_wrong_codec_id() {
            let mut bytes = valid_header_bytes(0);
            bytes[0] = 8; // codec_id=8 byte, not 3
            assert!(parse_header(&bytes).is_err());
        }

        #[test]
        fn parse_rejects_unsupported_mode() {
            for bad in [2u8, 3, 4, 0xFF] {
                let bytes = valid_header_bytes(bad);
                assert!(
                    parse_header(&bytes).is_err(),
                    "mode={bad} should be rejected"
                );
            }
        }

        #[test]
        fn parse_rejects_oversized_track_count() {
            let mut bytes = valid_header_bytes(0);
            bytes[8..12].copy_from_slice(&1000u32.to_le_bytes());
            assert!(parse_header(&bytes).is_err());
        }

        #[test]
        fn parse_accepts_mode_0_and_1() {
            assert!(matches!(
                parse_header(&valid_header_bytes(0)).unwrap().mode,
                Mode::PlanarMono
            ));
            assert!(matches!(
                parse_header(&valid_header_bytes(1)).unwrap().mode,
                Mode::InterleavedStereo
            ));
        }

        /// `decode_file` requires at least `HEADER_BYTES` of input but
        /// otherwise tolerates a payload of any length (including empty).
        #[test]
        fn decode_file_empty_payload_decodes_zero_samples() {
            let bytes = valid_header_bytes(0);
            let pcm = decode_file(&bytes).expect("decode");
            assert!(pcm.is_empty());
        }
    }
}

/// Codec id 8 -- proto-only DARE-IMA variant. Replaces codec_id=3 in
/// the Sep-13-2002 prototype build (retail uses codec_id=3 for the
/// equivalent files). The proto's `sub_199f90` dispatches per-nibble
/// to a 4-bit kernel (`sub_1999d0`) with an MMX bit-unpacker
/// (`sub_199f10`). All fields and dispatch paths are
/// reverse-engineered from the binary; see `Header` for the
/// file-header semantics and `decode_nibble_4bit` for the kernel
/// port.
///
/// File-header layout (12 dwords / 48 bytes; fields documented on the
/// `Header` struct):
///   +0x00  codec_id          = 8
///   +0x04  total_size        per-bank interleaved sample count
///   +0x08  outer_calls       dispatcher invocations per bank
///   +0x0c  tail              partial-chunk nibble count
///   +0x10  block_size        = 1536
///   +0x14  channels          1, 2, 4, or 6
///   +0x18  sample_rate       typically 36000 Hz
///   +0x1c  separate_flag     channel storage mode
///   +0x20  reserved_20       unused
///   +0x24  bits_per_sample   = 4 (matches kernel selection)
///   +0x28  kernel_selector   1 = 4-bit (sub_1999d0), else 6-bit (sub_199dc0)
///   +0x2c  dispatch_subtype  1 = mono, 2 = stereo, 4/6 = multichannel
///
/// State-resync block (52 bytes per channel, follows the file header
/// then again every macroblock). Layout per channel (read by the
/// 4-bit kernel as a 52-byte struct):
///   +0x00  u32   marker (= 2)
///   +0x04  i32  step_magnitude (clamped to [0x10f, 0xa00])
///   +0x08  i32  prev_hi_dot
///   +0x0c  i32  prev_prev_hi_dot
///   +0x10  2xi16 coef_lo  (low-tap pair filter coefficients)
///   +0x18  4xi16 coef_hi  (high-tap quad coefficients)
///   +0x20  2xi16 hist_pred  (predictor sample history)
///   +0x28  4xi16 hist_delta (delta sample history)
///   +0x30  i16  delta_save
///
/// Constant tables (proto VAs):
///   0x2aebc8  4-bit step-magnitude table (8 entries, indexed by
///             |nibble - 7|; entries [1..=7] are the real bases)
///   0x2aec08  4-bit step-output table (8 entries)
///   0x2aecc8  4-bit step-magnitude-update table (66 entries)
///   0x2aeed8  Main signed lookup (66 entries; sign-half x 33)
///   0x2af000..0x2af030  MMX qword constants for the state update
///   0x2aefe0..0x2af000  Auxiliary MMX qword constants
pub mod codec8 {
    use super::units::Nibble;
    use super::units::SampleRate;
    use std::io;
    use std::marker::PhantomData;

    /// Codec selector at file +0x00.
    pub const CODEC_ID: u32 = 8;

    /// Channel count carried in the file header at `+0x14` and used
    /// by `sub_19b4a0`'s `"Adpcm allows only sound files with 1, 2,
    /// 4 and 6 channels"` validator.
    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub enum Channels {
        Mono,
        Stereo,
        Quad,
        Six,
    }

    impl Channels {
        pub fn from_u32(v: u32) -> io::Result<Self> {
            match v {
                1 => Ok(Self::Mono),
                2 => Ok(Self::Stereo),
                4 => Ok(Self::Quad),
                6 => Ok(Self::Six),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("channels = {v}; engine accepts only 1, 2, 4, or 6"),
                )),
            }
        }
        pub const fn count(self) -> u16 {
            match self {
                Self::Mono => 1,
                Self::Stereo => 2,
                Self::Quad => 4,
                Self::Six => 6,
            }
        }
    }

    /// Per-nibble kernel selected by `sub_199f90`'s prologue from
    /// `codec_params[+0x04]` (= file `+0x28`).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum KernelKind {
        FourBit,
        SixBit,
    }

    impl KernelKind {
        pub const fn from_u32(v: u32) -> Self {
            if v == 1 { Self::FourBit } else { Self::SixBit }
        }
    }

    /// Dispatcher mode at file `+0x2c`; selects which arm of
    /// `sub_199f90`'s switch handles the bank. Mirrors `Channels`
    /// in every observed file but is logically distinct (the engine
    /// allows `dispatch_subtype != channels` and just routes
    /// differently through the multichannel arms).
    pub type DispatchSubtype = Channels;

    /// Typestate marker for `Header<C>` indicating the file's
    /// `dispatch_subtype` is a specific variant of `Channels`. The
    /// trait is sealed to this module's marker structs so callers
    /// can't fabricate header tags.
    pub trait SubtypeMarker: sealed::Sealed {
        const SUBTYPE: Channels;
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MonoTag;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct StereoTag;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct QuadTag;
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct SixTag;

    mod sealed {
        pub trait Sealed {}
        impl Sealed for super::MonoTag {}
        impl Sealed for super::StereoTag {}
        impl Sealed for super::QuadTag {}
        impl Sealed for super::SixTag {}
    }

    impl SubtypeMarker for MonoTag {
        const SUBTYPE: Channels = Channels::Mono;
    }
    impl SubtypeMarker for StereoTag {
        const SUBTYPE: Channels = Channels::Stereo;
    }
    impl SubtypeMarker for QuadTag {
        const SUBTYPE: Channels = Channels::Quad;
    }
    impl SubtypeMarker for SixTag {
        const SUBTYPE: Channels = Channels::Six;
    }
    /// Block size at file +0x10. Every observed codec_id=8 file uses
    /// this constant; deviation indicates a malformed or different-
    /// codec file.
    pub const BLOCK_BYTES: u32 = 0x600;
    /// Total header byte size -- the kernel begins reading compressed
    /// data immediately after.
    pub const HEADER_BYTES: usize = 0x40;

    /// Top-of-file metadata (12 dwords, file offsets 0x00..0x30).
    /// Field semantics derived from `sub_19b4a0`'s file-header-to-
    /// codec-params copy block at proto VA `0x19b6a8..0x19b6e0` and
    /// the dispatcher `sub_199f90`'s reads of those params:
    ///
    /// ```text
    ///   codec_params[+0x00] = file[+0x00]   codec_id
    ///   codec_params[+0x04] = file[+0x28]   kernel_selector
    ///   codec_params[+0x0c] = file[+0x2c]   dispatch_subtype
    ///   codec_params[+0x10] = file[+0x1c]   separate_flag
    ///   codec_params[+0x14] = file[+0x24]   bits_per_sample
    ///   codec_params[+0x18] = file[+0x10]   block_size
    ///   codec_params[+0x1c] = file[+0x14]   channels
    /// ```
    #[derive(Clone, Debug)]
    pub struct Header<S: SubtypeMarker> {
        /// `+0x04`. Per-bank total interleaved sample count
        /// (= `(outer_calls - 1) * block_size + tail`).
        pub total_size: u32,
        /// `+0x08`. Number of dispatcher invocations to play one
        /// bank: `outer_calls - 1` produce a full chunk each, the
        /// last one a partial chunk sized by `tail`.
        pub outer_calls: u32,
        /// `+0x0c`. Nibble count consumed by the bank's last
        /// (partial) dispatcher call.
        pub tail: u32,
        /// `+0x10`. Full-chunk nibble count per dispatcher call;
        /// always 1536 in observed files.
        pub block_size: u32,
        /// `+0x14`. Logical output channel count, validated against
        /// `Channels` by `parse_header_any`.
        pub channels: Channels,
        /// `+0x18`. Per-channel sample rate (typically 36000 Hz).
        pub sample_rate: SampleRate,
        /// `+0x1c`. "Separate mode" flag. `0` means channels are
        /// stored separately (one channel-state per kernel call);
        /// non-zero means packed. Validated only for `channels > 2`
        /// by the `"Only adpcm 4 Bits Separate work with sounds
        /// having more than 2 channels"` check; mono/stereo files
        /// load regardless of its value.
        pub separate_flag: u32,
        /// `+0x20`. Reserved/unused; engine never reads it. Always
        /// `0` in every observed file.
        pub reserved_20: u32,
        /// `+0x24`. Bits per encoded nibble. Always `4` in observed
        /// files; matches the `KernelKind` selection.
        pub bits_per_sample: u32,
        /// `+0x28`. Per-nibble kernel selector.
        pub kernel_kind: KernelKind,
        /// `+0x2c`. Dispatcher mode encoded into the type parameter:
        /// every concrete `Header<S>` corresponds to a specific
        /// `Channels` variant. Code that needs the runtime variant
        /// reads `S::SUBTYPE`.
        _subtype: PhantomData<S>,
    }

    impl<S: SubtypeMarker> Header<S> {
        pub const fn dispatch_subtype(&self) -> Channels {
            S::SUBTYPE
        }
    }

    /// Untagged variant of `Header<S>` produced by `parse_header`.
    /// Codec dispatch matches on this; the inner `Header<S>` is
    /// passed to the subtype-specific decoder.
    #[derive(Clone, Debug)]
    pub enum HeaderAny {
        Mono(Header<MonoTag>),
        Stereo(Header<StereoTag>),
        Quad(Header<QuadTag>),
        Six(Header<SixTag>),
    }

    impl HeaderAny {
        pub const fn channels(&self) -> Channels {
            match self {
                Self::Mono(_) => Channels::Mono,
                Self::Stereo(_) => Channels::Stereo,
                Self::Quad(_) => Channels::Quad,
                Self::Six(_) => Channels::Six,
            }
        }

        pub const fn outer_calls(&self) -> u32 {
            match self {
                Self::Mono(h) => h.outer_calls,
                Self::Stereo(h) => h.outer_calls,
                Self::Quad(h) => h.outer_calls,
                Self::Six(h) => h.outer_calls,
            }
        }

        pub const fn tail(&self) -> u32 {
            match self {
                Self::Mono(h) => h.tail,
                Self::Stereo(h) => h.tail,
                Self::Quad(h) => h.tail,
                Self::Six(h) => h.tail,
            }
        }

        /// Bytes of state snapshot interleaved between macroblock pairs
        /// (one 52-byte state struct per channel).
        pub const fn state_pair_bytes(&self) -> usize {
            self.channels().count() as usize * 52
        }
    }

    fn parse_common<S: SubtypeMarker>(data: &[u8], channels: Channels) -> io::Result<Header<S>> {
        let read_u32 = |off: usize| u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        Ok(Header {
            total_size: read_u32(0x04),
            outer_calls: read_u32(0x08),
            tail: read_u32(0x0c),
            block_size: read_u32(0x10),
            channels,
            sample_rate: SampleRate::new(read_u32(0x18)),
            separate_flag: read_u32(0x1c),
            reserved_20: read_u32(0x20),
            bits_per_sample: read_u32(0x24),
            kernel_kind: KernelKind::from_u32(read_u32(0x28)),
            _subtype: PhantomData,
        })
    }

    pub fn parse_header(data: &[u8]) -> io::Result<HeaderAny> {
        if data.len() < HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "codec_id=8 header needs {HEADER_BYTES} bytes, got {}",
                    data.len()
                ),
            ));
        }
        let codec = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if codec != CODEC_ID {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected codec_id=8, got {codec}"),
            ));
        }
        let read_u32 = |off: usize| u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        let channels = Channels::from_u32(read_u32(0x14))?;
        let subtype = Channels::from_u32(read_u32(0x2c))?;
        Ok(match subtype {
            Channels::Mono => HeaderAny::Mono(parse_common(data, channels)?),
            Channels::Stereo => HeaderAny::Stereo(parse_common(data, channels)?),
            Channels::Quad => HeaderAny::Quad(parse_common(data, channels)?),
            Channels::Six => HeaderAny::Six(parse_common(data, channels)?),
        })
    }

    /// Detect a codec_id=8 file by its first 4 bytes. Distinguishes
    /// it from codec_id=3 (`first byte == 0x03`) and the multi-bank
    /// `02 00 00 00 03 00 00 00` STREAM.SS2 variant.
    pub fn is_codec8(data: &[u8]) -> bool {
        data.len() >= 4 && u32::from_le_bytes(data[0..4].try_into().unwrap()) == CODEC_ID
    }

    /// 4-bit step-magnitude table at proto VA 0x2aebc8. Indexed by
    /// `abs(nibble - 7)`; entries [1..=7] are the real step bases.
    /// Entry [0] is read for `nibble == 7` (mag = 0); the value here
    /// is what the engine reads at proto VA 0x2aebc4 (one dword
    /// before the table), and is needed for byte-perfect state
    /// updates even though the downstream sign check zeroes the
    /// emitted delta.
    pub const STEP_MAG_4BIT: [i32; 8] = [0xfa0a_1f00u32 as i32, 8, 269, 425, 545, 645, 745, 850];

    /// 4-bit step-output table at proto VA 0x2aec08, indexed by the
    /// per-call magnitude `abs(nibble - 7)` (range 0..8 in practice;
    /// mag=8 reads past the array but never affects observed audio).
    pub const STEP_OUT_4BIT: [i32; 8] = [
        -1536,       // 0xfffffa00
        0x0000_090a, // 2314
        0x0000_147b, // 5243
        0x0000_2000, // 8192
        0x0000_3800, // 14336
        0x0000_630a, // 25354
        0x0000_b185, // 45445
        0x0002_310a, // 143626
    ];

    /// Main 66-entry signed lookup at proto VA 0x2aeed8. Values run
    /// from +1024 -> +2007 (entries 0..32) and -1024 -> -2007 (entries
    /// 33..65), so `(eax & 0x84)` selects the sign half (0 vs 0x84
    /// = 33x4 in dword-indexed bytes, indexing the second half).
    pub const MAIN_LOOKUP: [i32; 66] = [
        1024, 1031, 1053, 1076, 1099, 1123, 1148, 1172, 1198, 1224, 1251, 1278, 1306, 1334, 1363,
        1393, 1423, 1454, 1485, 1518, 1551, 1584, 1619, 1654, 1690, 1726, 1764, 1802, 1841, 1881,
        1922, 1964, 2007, -1024, -1031, -1053, -1076, -1099, -1123, -1148, -1172, -1198, -1224,
        -1251, -1278, -1306, -1334, -1363, -1393, -1423, -1454, -1485, -1518, -1551, -1584, -1619,
        -1654, -1690, -1726, -1764, -1802, -1841, -1881, -1922, -1964, -2007,
    ];

    /// MMX memory constants at proto VA 0x2af000..0x2af030. Each
    /// 64-bit value is the qword the kernel loads at that address.
    /// Stored as `u64` of LE-packed `i16`/`i32` lanes per Intel MMX
    /// register layout.
    const MC_2AF000: u64 = packed_i16(-4096, -4096, -4096, -4096);
    const MC_2AF008: u64 = packed_i16(0, 1024, 0, 0);
    const MC_2AF010: u64 = packed_i16(-1, 0, 0, 0);
    const MC_2AF018: u64 = packed_i16(30719, 30719, 30720, 30720);
    const MC_2AF020: u64 = packed_i16(1, 1, 1, 1);
    const MC_2AF028: u64 = packed_i16(0x0c00, 0x00ff, 0x00fe, 0x0002);
    /// `[0x2aefe0].d` zero-extended to qword (only low 32 bits used).
    const MC_2AEFE0: u64 = (0x00fe_00ff_u32) as u64;
    const MC_2AEFE8: u64 = packed_i16(2048, 2048, 2048, 2048);
    const MC_2AEFF0: u64 = packed_i16(255, 255, 255, 255);
    const MC_2AEFF8: u64 = packed_i16(-1, -1, -1, -1);

    const fn packed_i16(a: i16, b: i16, c: i16, d: i16) -> u64 {
        (a as u16 as u64)
            | ((b as u16 as u64) << 16)
            | ((c as u16 as u64) << 32)
            | ((d as u16 as u64) << 48)
    }

    /// Per-channel decoder state mirroring the engine's runtime
    /// struct read by `sub_1942e0`. Each field's offset matches the
    /// engine's so future trace replay can map state captures back
    /// directly.
    ///
    /// The kernel is a 6-tap adaptive FIR predictor layered over an
    /// IMA-ADPCM-style step+magnitude reconstruction. Two parallel
    /// sample histories are maintained: `hist_pred[0..2]` (full
    /// predictor) and `hist_delta[0..4]` (bare IMA delta term).
    #[derive(Clone, Debug, Default)]
    pub struct ChannelState {
        /// Engine `+0x04`. Running "step base" added to the per-
        /// nibble bucket value; clamped to `[0x10f, 0xa00]`.
        pub step_magnitude: i32,
        /// Engine `+0x08`. Previous call's high-tap dot product
        /// result (kept for inlined-path use only; the kernel itself
        /// doesn't read it as an input).
        pub prev_hi_dot: i32,
        /// Engine `+0x0c`. Two-back history of `prev_hi_dot`.
        pub prev_prev_hi_dot: i32,
        /// Engine `+0x10` and `+0x12`. Filter taps 0 and 1. Clamped
        /// after every call: `coef[1] ∈ [-0x300, +0x300]`, then
        /// `coef[0] ∈ [-(0x3c0 - |coef[1]|), +(0x3c0 - |coef[1]|)]`.
        pub coef_lo: [i16; 2],
        /// Engine `+0x18..+0x20`. Filter taps 2..6 (4 taps held as a
        /// qword for parallel MMX dot product).
        pub coef_hi: [i16; 4],
        /// Engine `+0x20..+0x24`. History samples paired with the
        /// low-tap pair. `hist_pred[0]` = newest, `hist_pred[1]` =
        /// one-back.
        pub hist_pred: [i16; 2],
        /// Engine `+0x28..+0x30`. History samples paired with the
        /// high-tap quad; oldest first to match MMX lane order.
        pub hist_delta: [i16; 4],
        /// Engine `+0x2e` snapshotted to `+0x30` at call start, then
        /// updated. Holds the last-cycle delta value for inter-call
        /// state continuity. (Role still TBD pending trace replay.)
        pub delta_save: i16,
    }

    /// Saturate an `i32` value to the `i16` range.
    #[inline]
    fn sat_i16(v: i32) -> i16 {
        v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
    }

    /// Scalar MMX primitives operating on 64-bit qwords. Each function
    /// matches the corresponding Intel MMX instruction's semantics
    /// bit-exactly so the per-instruction port of `sub_1999d0` below
    /// can be read line-for-line against the binary's LLIL.
    mod mmx {
        const M16: u64 = 0xFFFF;
        const M32: u64 = 0xFFFF_FFFF;

        #[inline]
        fn sat_i16_scalar(v: i32) -> i16 {
            v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
        }
        #[inline]
        fn s16(q: u64, i: u32) -> i16 {
            ((q >> (16 * i)) & M16) as u16 as i16
        }
        #[inline]
        fn s32(q: u64, i: u32) -> i32 {
            ((q >> (32 * i)) & M32) as u32 as i32
        }
        #[inline]
        pub fn pack_i16(a: i16, b: i16, c: i16, d: i16) -> u64 {
            (a as u16 as u64)
                | ((b as u16 as u64) << 16)
                | ((c as u16 as u64) << 32)
                | ((d as u16 as u64) << 48)
        }
        #[inline]
        pub fn pack_i32(a: i32, b: i32) -> u64 {
            (a as u32 as u64) | ((b as u32 as u64) << 32)
        }
        #[inline]
        pub fn lanes_i16(q: u64) -> [i16; 4] {
            [s16(q, 0), s16(q, 1), s16(q, 2), s16(q, 3)]
        }
        #[inline]
        pub fn lo32(q: u64) -> i32 {
            s32(q, 0)
        }

        #[inline]
        pub fn pmaddwd(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            let p0 = la[0] as i32 * lb[0] as i32 + la[1] as i32 * lb[1] as i32;
            let p1 = la[2] as i32 * lb[2] as i32 + la[3] as i32 * lb[3] as i32;
            pack_i32(p0, p1)
        }
        #[inline]
        pub fn paddsw(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            pack_i16(
                sat_i16_scalar(la[0] as i32 + lb[0] as i32),
                sat_i16_scalar(la[1] as i32 + lb[1] as i32),
                sat_i16_scalar(la[2] as i32 + lb[2] as i32),
                sat_i16_scalar(la[3] as i32 + lb[3] as i32),
            )
        }
        #[inline]
        pub fn psubsw(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            pack_i16(
                sat_i16_scalar(la[0] as i32 - lb[0] as i32),
                sat_i16_scalar(la[1] as i32 - lb[1] as i32),
                sat_i16_scalar(la[2] as i32 - lb[2] as i32),
                sat_i16_scalar(la[3] as i32 - lb[3] as i32),
            )
        }
        #[inline]
        pub fn paddw(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            pack_i16(
                la[0].wrapping_add(lb[0]),
                la[1].wrapping_add(lb[1]),
                la[2].wrapping_add(lb[2]),
                la[3].wrapping_add(lb[3]),
            )
        }
        #[inline]
        pub fn psubw(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            pack_i16(
                la[0].wrapping_sub(lb[0]),
                la[1].wrapping_sub(lb[1]),
                la[2].wrapping_sub(lb[2]),
                la[3].wrapping_sub(lb[3]),
            )
        }
        #[inline]
        pub fn psrawi(a: u64, n: u32) -> u64 {
            let s = n.min(15);
            let la = lanes_i16(a);
            pack_i16(la[0] >> s, la[1] >> s, la[2] >> s, la[3] >> s)
        }
        #[inline]
        pub fn psradi(a: u64, n: u32) -> u64 {
            let s = n.min(31);
            pack_i32(s32(a, 0) >> s, s32(a, 1) >> s)
        }
        #[inline]
        pub fn pslldi(a: u64, n: u32) -> u64 {
            if n >= 32 {
                return 0;
            }
            let l0 = (s32(a, 0) as u32).wrapping_shl(n) as i32;
            let l1 = (s32(a, 1) as u32).wrapping_shl(n) as i32;
            pack_i32(l0, l1)
        }
        #[inline]
        pub fn psrldi(a: u64, n: u32) -> u64 {
            if n >= 32 {
                return 0;
            }
            let l0 = ((s32(a, 0) as u32) >> n) as i32;
            let l1 = ((s32(a, 1) as u32) >> n) as i32;
            pack_i32(l0, l1)
        }
        #[inline]
        pub fn psllqi(a: u64, n: u32) -> u64 {
            if n >= 64 { 0 } else { a << n }
        }
        #[inline]
        pub fn psrlqi(a: u64, n: u32) -> u64 {
            if n >= 64 { 0 } else { a >> n }
        }
        #[inline]
        pub fn pmullw(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            pack_i16(
                (la[0] as i32 * lb[0] as i32) as i16,
                (la[1] as i32 * lb[1] as i32) as i16,
                (la[2] as i32 * lb[2] as i32) as i16,
                (la[3] as i32 * lb[3] as i32) as i16,
            )
        }
        #[inline]
        pub fn pmulhw(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            pack_i16(
                ((la[0] as i32 * lb[0] as i32) >> 16) as i16,
                ((la[1] as i32 * lb[1] as i32) >> 16) as i16,
                ((la[2] as i32 * lb[2] as i32) >> 16) as i16,
                ((la[3] as i32 * lb[3] as i32) >> 16) as i16,
            )
        }
        #[inline]
        pub fn punpcklwd(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            pack_i16(la[0], lb[0], la[1], lb[1])
        }
        #[inline]
        pub fn punpckhdq(a: u64, b: u64) -> u64 {
            pack_i32(s32(a, 1), s32(b, 1))
        }
        #[inline]
        pub fn paddd(a: u64, b: u64) -> u64 {
            pack_i32(
                s32(a, 0).wrapping_add(s32(b, 0)),
                s32(a, 1).wrapping_add(s32(b, 1)),
            )
        }
        #[inline]
        pub fn packssdw(a: u64, b: u64) -> u64 {
            pack_i16(
                sat_i16_scalar(s32(a, 0)),
                sat_i16_scalar(s32(a, 1)),
                sat_i16_scalar(s32(b, 0)),
                sat_i16_scalar(s32(b, 1)),
            )
        }
        #[inline]
        pub fn pcmpgtw(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            pack_i16(
                if la[0] > lb[0] { -1 } else { 0 },
                if la[1] > lb[1] { -1 } else { 0 },
                if la[2] > lb[2] { -1 } else { 0 },
                if la[3] > lb[3] { -1 } else { 0 },
            )
        }
        /// `pandn dst, src` = `(!dst) & src`. Intel semantics, not the
        /// "AND with NOT of memory operand" that Binary Ninja's LLIL
        /// view shows.
        #[inline]
        pub fn pandn(a: u64, b: u64) -> u64 {
            (!a) & b
        }
        #[inline]
        pub fn paddusw(a: u64, b: u64) -> u64 {
            let la = lanes_i16(a);
            let lb = lanes_i16(b);
            let sat = |x: u32, y: u32| -> i16 { ((x + y).min(0xFFFF)) as u16 as i16 };
            pack_i16(
                sat(la[0] as u16 as u32, lb[0] as u16 as u32),
                sat(la[1] as u16 as u32, lb[1] as u16 as u32),
                sat(la[2] as u16 as u32, lb[2] as u16 as u32),
                sat(la[3] as u16 as u32, lb[3] as u16 as u32),
            )
        }
    }

    impl ChannelState {
        /// Initialise state matching the proto engine's per-channel
        /// state after `sub_1940b0` runs. Values verified against
        /// QEMU trace: kernel_captures[0].state_at_ecx shows
        /// step_magnitude = 1280, all coefs and history = 0.
        pub fn init() -> Self {
            Self {
                step_magnitude: 1280,
                ..Self::default()
            }
        }
    }

    /// Decode one 4-bit nibble (0..15) into one i16 PCM sample,
    /// updating `state` in place. Faithful per-instruction port of
    /// the proto MMX kernel `sub_1999d0`, validated byte-perfect
    /// against 2048/2048 captured calls from a QEMU-plugin trace.
    pub fn decode_nibble_4bit(state: &mut ChannelState, nibble: Nibble) -> i16 {
        use mmx::*;
        let nibble = nibble.value();

        // The kernel views per-channel state as a 128-byte memory
        // region. Materialise the qword/dword views the kernel reads.
        let cl_lo_dword: u32 =
            (state.coef_lo[0] as u16 as u32) | ((state.coef_lo[1] as u16 as u32) << 16);
        let hp_dword: u32 =
            (state.hist_pred[0] as u16 as u32) | ((state.hist_pred[1] as u16 as u32) << 16);
        let slow_qword: u64 = pack_i16(
            state.coef_hi[0],
            state.coef_hi[1],
            state.coef_hi[2],
            state.coef_hi[3],
        );
        let hd_qword: u64 = pack_i16(
            state.hist_delta[0],
            state.hist_delta[1],
            state.hist_delta[2],
            state.hist_delta[3],
        );
        let prev_dot_qword: u64 =
            (state.prev_hi_dot as u32 as u64) | ((state.prev_prev_hi_dot as u32 as u64) << 32);
        let entry_step_mag = state.step_magnitude;

        // Dispatcher (0x1999d0..0x199ad9)
        let mut mm0 = cl_lo_dword as u64; // 0x1999e6 mm0 = zx.q([eax+0x10].d)
        let mut mm6 = hp_dword as u64; // 0x1999ea mm6 = zx.q([eax+0x20].d)
        let mut mm2 = slow_qword; // 0x1999ee mm2 = [eax+0x18].q
        let mut mm4 = hd_qword; // 0x1999f2 mm4 = [eax+0x28].q
        mm0 = pmaddwd(mm0, mm6); // 0x1999f6
        let centered: i32 = (nibble & 0xF) as i32 - 7; // 0x1999f9 ebx -= 7
        let si_save: i16 = state.hist_delta[3]; // 0x199a07 si = [eax+0x2e].w
        let mut mm3 = hd_qword; // 0x199a0d
        mm0 = psradi(mm0, 0xa); // 0x199a11 (dot_lo >>= 10)
        let mag = centered.unsigned_abs() as i32; // 0x199a15 ebx = abs(centered)
        mm2 = pmaddwd(mm2, mm3); // 0x199a17
        let dot_lo_lo32: i32 = lo32(mm0); // 0x199a29 [ebp-0x10] = mm0.d
        let mut mm1 = mm2; // 0x199a2d
        // `mag` ranges 0..=7 for nibbles 0..=14; nibble == 15 would
        // produce mag == 8 (an OOB read in the engine). No real audio
        // ever produces that input (verified across the entire music
        // and dialog trace, 96k+ nibbles), so we clamp here rather
        // than carry the OOB value as a guard entry.
        let step_full = STEP_MAG_4BIT[(mag as usize).min(7)].wrapping_add(entry_step_mag); // 0x199a30 ebx += ecx
        mm1 = punpckhdq(mm1, mm2); // 0x199a32
        let step_aligned: i32 = (step_full as u32 & 0xFFFF_FF00) as i32; // 0x199a3a ebx &= 0xffffff00
        mm1 = paddd(mm1, mm2); // 0x199a37
        mm1 = psradi(mm1, 0xa); // 0x199a40
        let sign_half: usize = if centered < 0 { 33 } else { 0 }; // 0x199a4f eax &= 0x84
        let step_lo_idx: usize = (((step_full - step_aligned) >> 3) & 0xFF) as usize; // 0x199a47 + 0x199a54
        let main_val: i32 = MAIN_LOOKUP
            .get(sign_half + step_lo_idx)
            .copied()
            .unwrap_or(0); // 0x199a67
        let cl_shift: u32 = ((step_aligned as u32) >> 8) & 0x1F; // 0x199a64 (signed >> 8, then low 5 bits)
        let dot_hi_lo32: i32 = lo32(mm1); // 0x199a59 [ebp-4] = mm1.d
        mm4 = psllqi(mm4, 0x10); // 0x199a5d
        let shifted: i32 = (main_val as u32).wrapping_shl(cl_shift) as i32; // 0x199a70 edx <<= cl
        let shifted: i32 = shifted >> 0xa; // 0x199a72 edx >>= 10 (signed)
        let mask: i32 = if step_aligned >= 1 { -1 } else { 0 }; // 0x199a6a + 0x199a75
        let delta: i32 = if mask == -1 { shifted } else { 0 }; // 0x199a7b
        let result_i32: i32 = dot_hi_lo32.wrapping_add(delta).wrapping_add(dot_lo_lo32); // 0x199a80 + 0x199a82
        let var_8_dot_hi: i32 = dot_hi_lo32; // saved as var_8 = mm1.d (pre-paddw)

        let mut mm5: u64 = result_i32 as u32 as u64; // 0x199a87 mm5 = zx.q(result)
        mm6 = psllqi(mm6, 0x10); // 0x199a8a
        mm0 = delta as u32 as u64; // 0x199a8e mm0 = zx.q(delta)
        mm5 = pslldi(mm5, 0x10); // 0x199a92
        mm2 = prev_dot_qword; // 0x199a96 mm2 = [eax+8].q
        mm0 = pslldi(mm0, 0x10); // 0x199a9a
        mm3 = MC_2AF028; // 0x199a9e
        mm5 = psrldi(mm5, 0x10); // 0x199aa5
        mm0 = psrldi(mm0, 0x10); // 0x199aa9
        mm5 |= mm6; // 0x199aad
        mm4 |= mm0; // 0x199ab0
        mm1 = paddw(mm1, mm0); // 0x199ab3
        let new_hp_dword: u32 = lo32(mm5) as u32; // 0x199ab6 [eax+0x20].d = mm5.d
        mm2 = packssdw(mm2, mm5); // 0x199aba
        let new_hd_qword: u64 = mm4; // 0x199abd [eax+0x28].q = mm4
        mm2 = psllqi(mm2, 0x10); // 0x199ac1
        let new_delta_save: i16 = si_save; // 0x199ac5 [eax+0x30].w = si
        let branch_test: u32 = lo32(mm1) as u32 & 0xFFFF; // 0x199ad3 ebx &= 0xffff
        mm1 = pslldi(mm1, 0x10); // 0x199acf
        let is_silence = branch_test == 0; // 0x199ad9

        // Both branches converge at 0x199cf1 with mm1 (going into the
        // saturation ladder), mm2 (new slow_filter), and a new
        // step_magnitude. Compute these via whichever branch applies.
        let mut new_step_mag: i32;
        if is_silence {
            // Branch A (silence path, 0x199c2e..0x199cee).
            mm1 = pack_i16(state.coef_lo[0], state.coef_lo[1], 0, 0); // 0x199c2e mm1 = [eax+0x10].q
            mm3 = MC_2AEFE0; // 0x199c32 (255, 254, 0, 0)
            mm2 = mm1; // 0x199c39
            mm1 = pmullw(mm1, mm3); // 0x199c3c
            mm2 = pmulhw(mm2, mm3); // 0x199c47
            mm1 = punpcklwd(mm1, mm2); // 0x199c4a
            mm1 = psradi(mm1, 8); // 0x199c4d
            mm1 = packssdw(mm1, mm3); // 0x199c51
            let mut mm0a: u64 = new_hd_qword; // 0x199c54 mm0 = [esi+0x28].q
            let mut mm7a: u64 = entry_step_mag as u32 as u64; // 0x199c58 mm7 = zx.q([esi+4].d)
            mm0a = psllqi(mm0a, 0x30); // 0x199c5c
            let mm4a: u64 = 0xf6; // 0x199c68 mm4 = ecx (ecx=0xf6)
            let mm6a_save: u64 = mm0a; // 0x199c6b mm6 = mm0
            mm0a = psrlqi(mm0a, 0x20); // 0x199c6e
            let stepout = STEP_OUT_4BIT[mag.min(7) as usize];
            mm0a |= mm6a_save; // 0x199c79
            // Post-write read of state+0x2a..0x31 (new hd + delta_save).
            let post_hd = lanes_i16(new_hd_qword);
            let mm6a_2a = pack_i16(post_hd[1], post_hd[2], post_hd[3], new_delta_save);
            mm0a = psradi(mm0a, 0x20); // 0x199c80
            mm0a &= MC_2AF000; // 0x199c84
            mm7a = pmaddwd(mm7a, mm4a); // 0x199c8b (step_mag.lo16 * 0xf6)
            mm2 = MC_2AEFF0; // 0x199c8e (255, 255, 255, 255)
            let mm6a_cmp = pcmpgtw(mm6a_2a, MC_2AEFF8); // 0x199c95
            let mm6a_cmp = pandn(mm6a_cmp, MC_2AEFF8); // 0x199c9c
            mm2 = pmullw(mm2, slow_qword); // 0x199ca3
            mm0a |= MC_2AEFE8; // 0x199ca7
            let mm6a_cmp = paddusw(mm6a_cmp, MC_2AF020); // 0x199cae
            let edi_step: i32 = lo32(mm7a); // 0x199cb5
            mm0a = pmullw(mm0a, mm6a_cmp); // 0x199cb8
            new_step_mag = stepout.wrapping_add(edi_step) >> 8; // 0x199cbd
            mm2 = paddsw(mm2, mm0a); // 0x199ccd
            new_step_mag = new_step_mag.clamp(0x10F, 0xA00); // 0x199cc6..0x199cdc
            mm2 = psrawi(mm2, 8); // 0x199cd6
            mm6 = 0; // 0x199cee
        } else {
            // Branch B (LMS path, 0x199adf..0x199c29).
            let mut mm0b: u64 = pack_i16(state.coef_lo[0], state.coef_lo[1], 0, 0); // 0x199adf
            mm1 = psradi(mm1, 0x1f); // 0x199ae3
            mm1 |= MC_2AF020; // 0x199ae7
            mm4 = paddsw(new_hd_qword, mm2); // 0x199aee (mm4 = new_hd; mm2 = packssdw output)
            mm4 = psrawi(mm4, 0xf); // 0x199af1
            mm5 = mm3; // 0x199af5 (mm3 was MC_2AF028)
            mm4 |= MC_2AF020; // 0x199af8
            mm0b = pslldi(mm0b, 0x10); // 0x199aff (Binary Ninja's LLIL view missed this; verified via raw bytes)
            let mut mm7b: u64 = MC_2AF018; // 0x199b03
            mm4 = psrlqi(mm4, 0x10); // 0x199b0a
            mm4 = pmullw(mm4, mm1); // 0x199b0e
            mm1 = pack_i16(state.coef_lo[0], state.coef_lo[1], 0, 0); // 0x199b14 mm1 = [eax+0x10].q
            mm5 = psrlqi(mm5, 0x20); // 0x199b18
            mm1 = psllqi(mm1, 0x20); // 0x199b1c
            mm2 = mm4; // 0x199b25
            mm1 = psrlqi(mm1, 0x30); // 0x199b28
            mm4 &= MC_2AF010; // 0x199b2c
            mm4 |= mm0b; // 0x199b33
            let mut mm6b: u64 = entry_step_mag as u32 as u64; // 0x199b39 mm6 = zx.q([esi+4].d)
            mm3 = pmaddwd(mm3, mm4); // 0x199b3d
            let mm4_ecx: u64 = 0xf6; // 0x199b4c mm4 = ecx (ecx=0xf6)
            mm6b = pmaddwd(mm6b, mm4_ecx); // 0x199b4f
            mm3 = psrldi(mm3, 8); // 0x199b52
            mm4 = mm3; // 0x199b56
            mm3 = pslldi(mm3, 2); // 0x199b59
            let edi_step: i32 = lo32(mm6b); // 0x199b5d
            mm3 = paddsw(mm3, mm7b); // 0x199b60
            mm3 = psubsw(mm3, mm7b); // 0x199b63
            mm7b = psrlqi(mm7b, 0x20); // 0x199b66
            let mut mm0c: u64 = new_hd_qword; // 0x199b6a
            mm3 = psubsw(mm3, mm7b); // 0x199b6e
            mm0c = psllqi(mm0c, 0x30); // 0x199b71
            mm3 = paddsw(mm3, mm7b); // 0x199b75
            let mm6c_save: u64 = mm0c; // 0x199b78
            mm3 &= MC_2AF010; // 0x199b7b
            mm0c = psrlqi(mm0c, 0x20); // 0x199b82
            mm3 |= MC_2AF008; // 0x199b86
            mm0c |= mm6c_save; // 0x199b8d
            mm3 = pmullw(mm3, mm2); // 0x199b90
            mm2 = mm3; // 0x199b9c
            mm0c = psradi(mm0c, 0x20); // 0x199b95
            mm0c &= MC_2AF000; // 0x199b9f
            mm2 = psrlqi(mm2, 0x10); // 0x199ba6
            let post_hd = lanes_i16(new_hd_qword);
            let mm6c_2a = pack_i16(post_hd[1], post_hd[2], post_hd[3], new_delta_save); // 0x199baa [esi+0x2a].q
            mm2 = psubw(mm2, mm3); // 0x199bae
            let mm6c_cmp = pcmpgtw(mm6c_2a, MC_2AEFF8); // 0x199bb1
            mm2 = pslldi(mm2, 0x10); // 0x199bb8
            let mm6c_cmp = pandn(mm6c_cmp, MC_2AEFF8); // 0x199bbc
            mm1 |= mm2; // 0x199bc3
            let mm6c_cmp = paddusw(mm6c_cmp, MC_2AF020); // 0x199bc6
            mm1 = pmaddwd(mm1, mm5); // 0x199bcd
            let stepout = STEP_OUT_4BIT[mag.min(7) as usize];
            new_step_mag = stepout.wrapping_add(edi_step) >> 8; // 0x199bcd..0x199bd6 ebx s>>= 8
            new_step_mag = new_step_mag.clamp(0x10F, 0xA00); // 0x199bd6..0x199be5
            mm2 = MC_2AEFF0; // 0x199bed
            mm1 = psradi(mm1, 8); // 0x199bf4
            mm0c |= MC_2AEFE8; // 0x199bf8
            mm1 = pslldi(mm1, 0x10); // 0x199bff
            mm2 = pmullw(mm2, slow_qword); // 0x199c03 [esi+0x18] = old slow_filter
            mm4 = pslldi(mm4, 0x10); // 0x199c07
            mm0c = pmullw(mm0c, mm6c_cmp); // 0x199c15
            mm6 = 0; // 0x199c18
            mm4 = psrldi(mm4, 0x10); // 0x199c1b
            mm2 = paddsw(mm2, mm0c); // 0x199c1f
            mm1 |= mm4; // 0x199c22
            mm2 = psrawi(mm2, 8); // 0x199c25
        }

        // Saturation ladder (0x199cf1..0x199d3e), shared by both
        // branches. mm2 ends up written to coef_lo; mm1 going in is
        // the per-branch LMS chain output.
        let new_slow_qword = mm2;
        let mm4_lad: u64 = 0x7cff_0000u32 as u64; // 0x199cf5 mm4 = zx.q(eax)
        let mut mm2l = mm1; // 0x199cf8
        let mut mm1l = punpcklwd(mm1, mm6); // 0x199cfb
        mm2l = paddsw(mm2l, mm4_lad); // 0x199cfe
        let mm5_lad: u64 = 0x8300_0000u32 as u64; // 0x199d01 mm5 = zx.q(ebx)
        mm1l = psrlqi(mm1l, 0x20); // 0x199d09
        mm2l = psubsw(mm2l, mm4_lad); // 0x199d0d
        mm2l = paddsw(mm2l, mm5_lad); // 0x199d13
        let mm1d: i32 = lo32(mm1l); // 0x199d16 ebx = mm1.d
        mm2l = psubsw(mm2l, mm5_lad); // 0x199d19
        let ecx_clamp: i32 = 0x3c0i32.wrapping_sub(mm1d); // 0x199d1c
        let edx_clamp: i32 = 0x7fffi32.wrapping_sub(ecx_clamp); // 0x199d23
        let mm5_lad2: u64 = edx_clamp as u32 as u64; // 0x199d27 mm5 = zx.q(edx)
        let prev_dot1 = state.prev_hi_dot; // 0x199d2a eax = [esi+8].d
        mm2l = paddsw(mm2l, mm5_lad2); // 0x199d2d
        mm2l = psubsw(mm2l, mm5_lad2); // 0x199d33

        // Commit final state writes (0x199ce1 / 0x199bea / 0x199d36..).
        state.step_magnitude = new_step_mag;
        state.prev_prev_hi_dot = prev_dot1; // 0x199d36 [esi+0xc] = old [esi+8]
        state.prev_hi_dot = var_8_dot_hi; // 0x199d3a [esi+8] = var_8_1
        let cl_lo_new = lo32(mm2l) as u32; // 0x199d3e [esi+0x10].d = mm2.d
        state.coef_lo = [cl_lo_new as i16, (cl_lo_new >> 16) as i16];
        state.coef_hi = lanes_i16(new_slow_qword);
        let hp_lanes = lanes_i16(new_hp_dword as u64);
        state.hist_pred = [hp_lanes[0], hp_lanes[1]];
        state.hist_delta = lanes_i16(new_hd_qword);
        state.delta_save = new_delta_save;

        // Phase 7 tail clamps on coef_lo (0x199d44..0x199da7).
        let mut cl1 = state.coef_lo[1] as i32;
        if cl1.abs() > 0x300 {
            cl1 = if cl1 >= 0 { 0x300 } else { -0x300 };
            state.coef_lo[1] = cl1 as i16;
        }
        let cl0 = state.coef_lo[0] as i32;
        let bound = 0x3c0 - cl1;
        if cl0.abs() > bound {
            let sign = if cl0 >= 0 { 1 } else { -1 };
            state.coef_lo[0] = sat_i16(sign * bound);
        }

        // The kernel returns `eax = var_c = result_i32` (full 32 bits).
        // The caller (`sub_199f90` at 0x19a0f6) writes only `ax` -- the
        // low 16 bits -- to PCM. Truncate, do NOT saturate: when the
        // adaptive predictor overshoots, the engine's output wraps
        // around in i16, matching the value already stored to
        // `hist_pred[0]` here in the kernel.
        result_i32 as i16
    }

    /// Unpack 4-bit nibbles from a packed byte stream, matching the
    /// engine's `sub_199f10`. Each 8 source bytes form a qword
    /// `(lo_dword << 32) | hi_dword` (the two halves swap places),
    /// then 16 nibbles are extracted at bit positions
    /// `60, 56, 52, ..., 4, 0` (descending). Verified against
    /// kernel_captures `stack4` values from the QEMU trace.
    pub fn unpack_nibbles_4bit(input: &[u8], output: &mut [u8]) {
        let count = output.len();
        let mut written = 0usize;
        let mut src = 0usize;
        while written < count {
            assert!(
                src + 8 <= input.len(),
                "unpack_nibbles_4bit: input exhausted"
            );
            let lo = u32::from_le_bytes(input[src..src + 4].try_into().unwrap()) as u64;
            let hi = u32::from_le_bytes(input[src + 4..src + 8].try_into().unwrap()) as u64;
            let qword = (lo << 32) | hi;
            src += 8;
            for shift in (0..=60).rev().step_by(4) {
                if written >= count {
                    return;
                }
                output[written] = ((qword >> shift) & 0xF) as u8;
                written += 1;
            }
        }
    }

    /// Decode a whole codec_id=8 file (header + data) into PCM. The
    /// caller is responsible for splitting stereo output into L/R
    /// channels if the header reports `channels == 2`.
    ///
    /// PORT STATUS: the 4-bit kernel is a first-pass static port
    /// (Phases 4 and 6 are best-guess). Validation needs listening
    /// or a proto/demo runtime trace.
    /// Iterate the codec_id=8 banks of an `.LS2`/`.SS2` style
    /// container. A bank is a self-contained codec_id=8 file (its own
    /// 0x40-byte header + 0x24 zero pad + nibble-packed audio data).
    /// Banks are concatenated back-to-back: scan forward for the next
    /// plausible codec_id=8 header to find each bank's end.
    pub fn iter_banks(file_bytes: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
        let mut cursor = 0usize;
        std::iter::from_fn(move || {
            if cursor + HEADER_BYTES > file_bytes.len() {
                return None;
            }
            // Confirm cursor is on a real bank header.
            if !is_plausible_bank_header(file_bytes, cursor) {
                return None;
            }
            // Search forward for the next plausible bank header to
            // bound this bank. End of file otherwise.
            let search_start = cursor + HEADER_BYTES;
            let mut bank_end = file_bytes.len();
            let mut probe = search_start;
            while probe + HEADER_BYTES <= file_bytes.len() {
                if is_plausible_bank_header(file_bytes, probe) {
                    bank_end = probe;
                    break;
                }
                probe += 1;
            }
            let bytes = &file_bytes[cursor..bank_end];
            let bank_offset = cursor;
            cursor = bank_end;
            Some((bank_offset, bytes))
        })
    }

    /// Heuristic: a real codec_id=8 bank header has codec_id=8 at
    /// `+0x00`, block_size=1536 at `+0x10`, channels in {1, 2} at
    /// `+0x14`, a recognised sample rate at `+0x18`, and the
    /// init-step-magnitude constant `1280` at `+0x34`.
    fn is_plausible_bank_header(bytes: &[u8], at: usize) -> bool {
        if at + HEADER_BYTES > bytes.len() {
            return false;
        }
        let read = |o: usize| u32::from_le_bytes(bytes[at + o..at + o + 4].try_into().unwrap());
        let codec = read(0x00);
        let block = read(0x10);
        let channels = read(0x14);
        let rate = read(0x18);
        let init = read(0x34);
        codec == CODEC_ID
            && block == 0x600
            && (channels == 1 || channels == 2)
            && matches!(
                rate,
                8000 | 11025 | 16000 | 22050 | 32000 | 36000 | 44100 | 48000
            )
            && init == 1280
    }

    /// Decode the whole codec_id=8 file into PCM. Delegates to
    /// `decode_bank` since each `.LS2` is a single bank (the per-
    /// macro-block 52-byte resync headers are state snapshots, not
    /// sub-sound markers).
    pub fn decode_file(file_bytes: &[u8]) -> io::Result<(HeaderAny, Vec<i16>)> {
        let header = parse_header(file_bytes)?;
        let pcm = decode_bank(file_bytes)?;
        Ok((header, pcm))
    }

    /// Decode one codec_id=8 file. Two on-disc layouts are observed,
    /// distinguished by the `dispatch_subtype` field of the header
    /// (= 1 for voice/SFX `.LS2`, = 2 for stereo music `.SS2`):
    ///
    /// **Mono LS2** (`dispatch_subtype == 1`), 1590-byte macroblock stride:
    /// ```text
    ///   0..769     chunk A   (768 nibble bytes + 1 reserved)
    ///   769..1538  chunk B   (768 nibble bytes + 1 reserved)
    ///   1538..1590 52-byte mono state-resync header (skip)
    /// ```
    ///
    /// **Stereo SS2** (`dispatch_subtype == 2`), 1642-byte macroblock stride
    /// starting at file offset `0x30`:
    /// ```text
    ///   0..52     52-byte L channel state-resync header (skip)
    ///   52..104   52-byte R channel state-resync header (skip)
    ///   104..873  chunk_A (768 nibble bytes + 1 padding)
    ///   873..1642 chunk_B (768 nibble bytes + 1 padding)
    /// ```
    /// Each chunk is one `sub_199f90` dispatcher invocation: the
    /// unpacker produces 1536 nibbles interleaved as L,R,L,R; the
    /// inline case-1 loop applies the same 4-bit kernel (`sub_1999d0`)
    /// to L state (offset 0 of channel_state region) for even-indexed
    /// nibbles and R state (offset 52) for odd-indexed, writing 768
    /// stereo frames as interleaved L,R i16. Verified byte-perfect
    /// against the engine for all 64 captured music dispatches via
    /// `tests::stereo_pcm_matches_engine_output`.
    ///
    /// State flows continuously across all state-resync headers
    /// during sequential decode; we just skip them.
    pub fn decode_bank(file_bytes: &[u8]) -> io::Result<Vec<i16>> {
        let mut pcm = Vec::new();
        for bank in decode_each_bank(file_bytes)? {
            pcm.extend_from_slice(&bank.pcm);
        }
        Ok(pcm)
    }

    /// One decoded bank's resolved playback parameters and samples.
    /// Carries only the values a consumer needs for WAV output; the
    /// full `Header<S>` is dropped at the boundary so callers don't
    /// have to thread the typestate through.
    pub struct DecodedBank {
        pub channels: Channels,
        pub sample_rate: SampleRate,
        pub pcm: Vec<i16>,
    }

    /// Iterate every bank in a multi-bank codec_id=8 file and decode
    /// each independently. Each `DecodedBank.pcm` is one continuous
    /// audio clip (interleaved L,R for stereo, mono for mono).
    pub fn decode_each_bank(file_bytes: &[u8]) -> io::Result<Vec<DecodedBank>> {
        let mut banks = Vec::new();
        let mut offset = 0usize;
        while offset + HEADER_BYTES <= file_bytes.len() {
            let codec_id = u32::from_le_bytes(file_bytes[offset..offset + 4].try_into().unwrap());
            if codec_id != CODEC_ID {
                break;
            }
            let header = parse_header(&file_bytes[offset..])?;
            let bank_size = compute_bank_size(&header);
            let bank_end = (offset + bank_size).min(file_bytes.len());
            let bank_bytes = &file_bytes[offset..bank_end];
            let bank = match header {
                HeaderAny::Mono(h) => DecodedBank {
                    channels: Channels::Mono,
                    sample_rate: h.sample_rate,
                    pcm: decode_mono_bank(bank_bytes, &h)?,
                },
                HeaderAny::Stereo(h) => DecodedBank {
                    channels: Channels::Stereo,
                    sample_rate: h.sample_rate,
                    pcm: decode_stereo_bank(bank_bytes, &h)?,
                },
                HeaderAny::Quad(_) | HeaderAny::Six(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "codec_id=8 multichannel (4/6) decoder not yet ported",
                    ));
                }
            };
            banks.push(bank);
            if bank_size == 0 {
                break;
            }
            offset += bank_size;
        }
        Ok(banks)
    }

    /// Bank layout for both subtypes:
    ///   48-byte file header, then `outer_calls - 1` full chunks
    ///   packed into `floor((outer_calls-1)/2)` full macroblocks plus
    ///   0 or 1 leftover chunk, then a state-pair header and a
    ///   `tail`-sized partial chunk.
    /// Mono LS2 macroblock: `[52B state][769B chunk_A][769B chunk_B]` (1590B).
    /// Stereo SS2 macroblock: `[52B L state][52B R state][769B chunk_A][769B chunk_B]` (1642B).
    fn compute_bank_size(header: &HeaderAny) -> usize {
        const FILE_HEADER: usize = 0x30;
        const CHUNK_SIZE: usize = 769;
        let outer = header.outer_calls() as usize;
        let tail = header.tail() as usize;
        let state_pair = header.state_pair_bytes();
        let macroblock = state_pair + CHUNK_SIZE * 2;
        let full_chunks = outer.saturating_sub(1);
        let full_macros = full_chunks / 2;
        let leftover_full_chunks = full_chunks - full_macros * 2;
        let partial_chunk_bytes = 1 + tail / 2;
        FILE_HEADER
            + full_macros * macroblock
            + state_pair
            + leftover_full_chunks * CHUNK_SIZE
            + partial_chunk_bytes
    }

    /// Decode a partial chunk of `nib_count` nibbles into pcm, with
    /// zero-padded input if the on-disc chunk has fewer source bytes
    /// than the unpacker's 8-byte qword granularity requires.
    fn decode_partial_chunk(
        bank: &[u8],
        chunk_off: usize,
        nib_count: usize,
        state_l: &mut ChannelState,
        state_r: &mut ChannelState,
        pcm: &mut Vec<i16>,
    ) {
        let src_bytes_needed = nib_count.div_ceil(16) * 8;
        let avail = bank.len().saturating_sub(chunk_off);
        let mut src = vec![0u8; src_bytes_needed];
        let copy = src_bytes_needed.min(avail);
        src[..copy].copy_from_slice(&bank[chunk_off..chunk_off + copy]);
        let mut nibs = vec![0u8; nib_count];
        unpack_nibbles_4bit(&src, &mut nibs);
        for pair in nibs.chunks_exact(2) {
            pcm.push(decode_nibble_4bit(state_l, Nibble::new(pair[0])));
            pcm.push(decode_nibble_4bit(state_r, Nibble::new(pair[1])));
        }
    }

    fn decode_mono_bank(bank_bytes: &[u8], header: &Header<MonoTag>) -> io::Result<Vec<i16>> {
        const FILE_HEADER: usize = 0x30;
        const STATE_HEADER_BYTES: usize = 52;
        const NIBBLES_PER_CHUNK: usize = 1536;
        const SRC_BYTES_PER_CHUNK: usize = NIBBLES_PER_CHUNK / 2;
        const FILE_CHUNK_STRIDE: usize = SRC_BYTES_PER_CHUNK + 1;
        const MACROBLOCK_BYTES: usize = STATE_HEADER_BYTES + FILE_CHUNK_STRIDE * 2;
        if bank_bytes.len() < FILE_HEADER + STATE_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "codec_id=8 bank too short for state header",
            ));
        }
        let outer_calls = header.outer_calls as usize;
        let tail_nibbles = header.tail as usize;
        let full_chunks = outer_calls.saturating_sub(1);
        let full_macros = full_chunks / 2;
        let leftover_full_chunks = full_chunks - full_macros * 2;
        let mut pcm = Vec::with_capacity(header.total_size as usize);
        let mut state = ChannelState::init();
        let mut nibbles = vec![0u8; NIBBLES_PER_CHUNK];
        let mut macroblock_start = FILE_HEADER;
        for _ in 0..full_macros {
            let chunk_a = macroblock_start + STATE_HEADER_BYTES;
            for chunk_off in [chunk_a, chunk_a + FILE_CHUNK_STRIDE] {
                unpack_nibbles_4bit(
                    &bank_bytes[chunk_off..chunk_off + SRC_BYTES_PER_CHUNK],
                    &mut nibbles,
                );
                for &n in &nibbles[..] {
                    pcm.push(decode_nibble_4bit(&mut state, Nibble::new(n)));
                }
            }
            macroblock_start += MACROBLOCK_BYTES;
        }
        let mut chunk_off = macroblock_start + STATE_HEADER_BYTES;
        for _ in 0..leftover_full_chunks {
            unpack_nibbles_4bit(
                &bank_bytes[chunk_off..chunk_off + SRC_BYTES_PER_CHUNK],
                &mut nibbles,
            );
            for &n in &nibbles[..] {
                pcm.push(decode_nibble_4bit(&mut state, Nibble::new(n)));
            }
            chunk_off += FILE_CHUNK_STRIDE;
        }
        if tail_nibbles > 0 {
            let src_bytes_needed = tail_nibbles.div_ceil(16) * 8;
            let avail = bank_bytes.len().saturating_sub(chunk_off);
            let mut src = vec![0u8; src_bytes_needed];
            let copy = src_bytes_needed.min(avail);
            src[..copy].copy_from_slice(&bank_bytes[chunk_off..chunk_off + copy]);
            let mut tail_nibs = vec![0u8; tail_nibbles];
            unpack_nibbles_4bit(&src, &mut tail_nibs);
            for &n in &tail_nibs {
                pcm.push(decode_nibble_4bit(&mut state, Nibble::new(n)));
            }
        }
        Ok(pcm)
    }

    fn decode_stereo_bank(bank_bytes: &[u8], header: &Header<StereoTag>) -> io::Result<Vec<i16>> {
        const HEADER_BYTES_BANK: usize = 0x30;
        const STATE_HEADER_BYTES: usize = 52;
        const STATE_PAIR_BYTES: usize = STATE_HEADER_BYTES * 2;
        const NIBBLES_PER_CHUNK: usize = 1536;
        const SRC_BYTES_PER_CHUNK: usize = NIBBLES_PER_CHUNK / 2;
        const FILE_CHUNK_STRIDE: usize = SRC_BYTES_PER_CHUNK + 1;
        const MACROBLOCK_BYTES: usize = STATE_PAIR_BYTES + FILE_CHUNK_STRIDE * 2;
        if bank_bytes.len() < HEADER_BYTES_BANK + STATE_PAIR_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "codec_id=8 stereo bank too short for state header",
            ));
        }
        let outer_calls = header.outer_calls as usize;
        let tail_nibbles = header.tail as usize;
        let full_chunks = outer_calls.saturating_sub(1);
        let full_macros = full_chunks / 2;
        let leftover_full_chunks = full_chunks - full_macros * 2;
        let mut pcm = Vec::with_capacity((header.total_size as usize) * 2);
        let mut state_l = ChannelState::init();
        let mut state_r = ChannelState::init();
        let mut nibs = vec![0u8; NIBBLES_PER_CHUNK];
        let mut macroblock_start = HEADER_BYTES_BANK;
        for _ in 0..full_macros {
            let chunk_a = macroblock_start + STATE_PAIR_BYTES;
            for chunk_off in [chunk_a, chunk_a + FILE_CHUNK_STRIDE] {
                unpack_nibbles_4bit(
                    &bank_bytes[chunk_off..chunk_off + SRC_BYTES_PER_CHUNK],
                    &mut nibs,
                );
                for pair in nibs.chunks_exact(2) {
                    pcm.push(decode_nibble_4bit(&mut state_l, Nibble::new(pair[0])));
                    pcm.push(decode_nibble_4bit(&mut state_r, Nibble::new(pair[1])));
                }
            }
            macroblock_start += MACROBLOCK_BYTES;
        }
        // Partial macroblock: state pair + (leftover_full_chunks) full chunks + 1 partial chunk.
        let mut chunk_off = macroblock_start + STATE_PAIR_BYTES;
        for _ in 0..leftover_full_chunks {
            unpack_nibbles_4bit(
                &bank_bytes[chunk_off..chunk_off + SRC_BYTES_PER_CHUNK],
                &mut nibs,
            );
            for pair in nibs.chunks_exact(2) {
                pcm.push(decode_nibble_4bit(&mut state_l, Nibble::new(pair[0])));
                pcm.push(decode_nibble_4bit(&mut state_r, Nibble::new(pair[1])));
            }
            chunk_off += FILE_CHUNK_STRIDE;
        }
        if tail_nibbles > 0 {
            decode_partial_chunk(
                bank_bytes,
                chunk_off,
                tail_nibbles,
                &mut state_l,
                &mut state_r,
                &mut pcm,
            );
        }
        Ok(pcm)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The 4-bit unpacker (`sub_199f10`) swaps the lo/hi dwords of
        /// each 8-byte source qword and extracts 16 nibbles at bit
        /// positions 60, 56, ..., 4, 0. Hand-computed reference:
        ///   lo = 0x6745_2301 (LE of `01 23 45 67`)
        ///   hi = 0xEFCD_AB89 (LE of `89 AB CD EF`)
        ///   qword = (lo << 32) | hi = 0x6745_2301_EFCD_AB89
        ///   -> nibbles MSB->LSB: 6,7,4,5,2,3,0,1,E,F,C,D,A,B,8,9
        #[test]
        fn unpacker_swaps_dwords_and_extracts_msb_first() {
            let input = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];
            let mut out = [0u8; 16];
            unpack_nibbles_4bit(&input, &mut out);
            assert_eq!(
                out,
                [
                    0x6, 0x7, 0x4, 0x5, 0x2, 0x3, 0x0, 0x1, 0xE, 0xF, 0xC, 0xD, 0xA, 0xB, 0x8, 0x9
                ]
            );
        }

        /// Output length not a multiple of 16 stops mid-qword without
        /// reading past the requested count. Asking for 10 nibbles must
        /// consume one 8-byte qword (which yields up to 16) and stop
        /// after writing 10.
        #[test]
        fn unpacker_stops_at_requested_count() {
            let input = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];
            let mut out = [0u8; 10];
            unpack_nibbles_4bit(&input, &mut out);
            assert_eq!(out, [0x6, 0x7, 0x4, 0x5, 0x2, 0x3, 0x0, 0x1, 0xE, 0xF]);
        }

        /// `parse_header` rejects short input.
        #[test]
        fn parse_header_short_input_errors() {
            assert!(parse_header(&[0u8; HEADER_BYTES - 1]).is_err());
        }

        /// `parse_header` rejects a non-codec-8 first dword.
        #[test]
        fn parse_header_wrong_codec_errors() {
            let mut data = vec![0u8; HEADER_BYTES];
            data[0..4].copy_from_slice(&3u32.to_le_bytes());
            assert!(parse_header(&data).is_err());
        }

        /// `parse_header` rejects forbidden channel counts. Picks `3`
        /// (the most likely off-by-one bug a future change would
        /// produce) and `0` plus `0xFFFF_FFFF`.
        #[test]
        fn parse_header_rejects_bad_channels() {
            for bad in [0u32, 3, 5, 0xFFFF_FFFF] {
                let mut data = vec![0u8; HEADER_BYTES];
                data[0..4].copy_from_slice(&CODEC_ID.to_le_bytes());
                data[0x14..0x18].copy_from_slice(&bad.to_le_bytes());
                data[0x2c..0x30].copy_from_slice(&1u32.to_le_bytes());
                assert!(parse_header(&data).is_err(), "channels={bad}");
            }
        }

        /// Valid headers parse and round-trip the subtype tag.
        #[test]
        fn parse_header_accepts_all_subtypes() {
            for (ch, want_count) in [(1u32, 1u16), (2, 2), (4, 4), (6, 6)] {
                let mut data = vec![0u8; HEADER_BYTES];
                data[0..4].copy_from_slice(&CODEC_ID.to_le_bytes());
                data[0x14..0x18].copy_from_slice(&ch.to_le_bytes());
                data[0x18..0x1c].copy_from_slice(&36000u32.to_le_bytes());
                data[0x28..0x2c].copy_from_slice(&1u32.to_le_bytes()); // kernel = 4-bit
                data[0x2c..0x30].copy_from_slice(&ch.to_le_bytes());
                let h = parse_header(&data).expect("parse");
                assert_eq!(h.channels().count(), want_count);
            }
        }

        fn state_from_bytes(b: &[u8]) -> ChannelState {
            let r16 = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]);
            let r32 = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
            ChannelState {
                step_magnitude: r32(0x04),
                prev_hi_dot: r32(0x08),
                prev_prev_hi_dot: r32(0x0c),
                coef_lo: [r16(0x10), r16(0x12)],
                coef_hi: [r16(0x18), r16(0x1a), r16(0x1c), r16(0x1e)],
                hist_pred: [r16(0x20), r16(0x22)],
                hist_delta: [r16(0x28), r16(0x2a), r16(0x2c), r16(0x2e)],
                delta_save: r16(0x30),
            }
        }

        fn state_to_bytes(s: &ChannelState, entry: &[u8]) -> Vec<u8> {
            let mut out = entry.to_vec();
            out[0x04..0x08].copy_from_slice(&s.step_magnitude.to_le_bytes());
            out[0x08..0x0c].copy_from_slice(&s.prev_hi_dot.to_le_bytes());
            out[0x0c..0x10].copy_from_slice(&s.prev_prev_hi_dot.to_le_bytes());
            out[0x10..0x12].copy_from_slice(&s.coef_lo[0].to_le_bytes());
            out[0x12..0x14].copy_from_slice(&s.coef_lo[1].to_le_bytes());
            for (i, v) in s.coef_hi.iter().enumerate() {
                out[0x18 + 2 * i..0x18 + 2 * i + 2].copy_from_slice(&v.to_le_bytes());
            }
            out[0x20..0x22].copy_from_slice(&s.hist_pred[0].to_le_bytes());
            out[0x22..0x24].copy_from_slice(&s.hist_pred[1].to_le_bytes());
            for (i, v) in s.hist_delta.iter().enumerate() {
                out[0x28 + 2 * i..0x28 + 2 * i + 2].copy_from_slice(&v.to_le_bytes());
            }
            out[0x30..0x32].copy_from_slice(&s.delta_save.to_le_bytes());
            out
        }

        /// `decode_nibble_4bit` should never leave `step_magnitude`
        /// outside the engine-validated clamp `[0x10f, 0xa00]` no
        /// matter what (state, nibble) pair drives it. This guards
        /// the clamp logic at the kernel tail and catches
        /// regressions where the clamp gets reordered or dropped.
        #[test]
        fn kernel_step_magnitude_stays_in_clamp() {
            const MIN: i32 = 0x10f;
            const MAX: i32 = 0xa00;
            // Initial state is well-formed; the engine never starts
            // outside the clamp.
            let mut state = ChannelState::init();
            for round in 0..256 {
                let nib = Nibble::new((round & 0xF) as u8);
                let _ = decode_nibble_4bit(&mut state, nib);
                assert!(
                    state.step_magnitude >= MIN && state.step_magnitude <= MAX,
                    "round {round}: step_magnitude {} out of clamp [{MIN}, {MAX}]",
                    state.step_magnitude
                );
            }
        }

        /// Decompress a gzipped fixture into a Vec. Re-used by the
        /// two trace-replay tests below.
        fn decompress_fixture(gz: &[u8]) -> Vec<u8> {
            use std::io::Read;
            let mut d = flate2::read::GzDecoder::new(gz);
            let mut out = Vec::new();
            d.read_to_end(&mut out).expect("decompress fixture");
            out
        }

        /// Byte-perfect replay of every recorded kernel transition from
        /// a QEMU plugin capture (2048 captures spanning all 15 used
        /// nibble values and a wide range of entry states). Drives the
        /// engine-captured pre-state through `decode_nibble_4bit` once
        /// and asserts the post-state bytes match the engine's
        /// recorded post-state. Regenerate the fixture with
        /// `tools/extract_audio_fixtures.py` after recapturing the
        /// trace.
        ///
        /// Replaces the previously `#[ignore]`d
        /// `byte_perfect_against_qemu_trace`: same coverage, but the
        /// data is a gzipped ~53 KB binary in the repo instead of a 27
        /// MB JSON on /var/tmp.
        #[test]
        fn kernel_transitions_byte_perfect() {
            const GZ: &[u8] = include_bytes!("../tests/fixtures/codec8_kernel_transitions.bin.gz");
            const RECORD_BYTES: usize = 1 + 52 + 52;
            let buf = decompress_fixture(GZ);
            let count = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
            assert_eq!(
                buf.len(),
                4 + count * RECORD_BYTES,
                "fixture has unexpected size for declared count={count}"
            );
            let mut failures: Vec<(usize, u8)> = Vec::new();
            for i in 0..count {
                let off = 4 + i * RECORD_BYTES;
                let nib = buf[off];
                let entry = &buf[off + 1..off + 53];
                let truth = &buf[off + 53..off + 105];
                let mut state = state_from_bytes(entry);
                let _ = decode_nibble_4bit(&mut state, Nibble::new(nib));
                let got = state_to_bytes(&state, entry);
                if got[..0x32] != truth[..0x32] {
                    failures.push((i, nib));
                }
            }
            assert!(
                failures.is_empty(),
                "{} / {count} kernel transition mismatches (first 5: {:?})",
                failures.len(),
                &failures[..failures.len().min(5)]
            );
        }

        /// Replay every captured outer dispatcher call (64 of them)
        /// through the full unpacker + L/R-alternating kernel and
        /// require the first `SAMPLES_PER_CALL` i16 samples to match
        /// the engine's recorded `pcm_output` exactly. Each call is
        /// replayed from its captured entry state -- no cross-call
        /// state continuity is implied or required.
        ///
        /// Replaces the previously `#[ignore]`d
        /// `stereo_pcm_matches_engine_output` with a self-contained
        /// version (~41 KB gzipped fixture).
        #[test]
        fn stereo_pcm_call_replay_matches_engine() {
            const GZ: &[u8] = include_bytes!("../tests/fixtures/codec8_stereo_calls.bin.gz");
            // Must match tools/extract_audio_fixtures.py.
            const SAMPLES_PER_CALL: usize = 256;
            const BLOCK_BYTES: usize = SAMPLES_PER_CALL / 2;
            const STATE_BYTES: usize = 104;
            const PCM_BYTES: usize = SAMPLES_PER_CALL * 2;
            const RECORD_BYTES: usize = BLOCK_BYTES + STATE_BYTES + PCM_BYTES;

            let buf = decompress_fixture(GZ);
            let count = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
            assert_eq!(
                buf.len(),
                4 + count * RECORD_BYTES,
                "fixture has unexpected size for declared count={count}"
            );
            let mut mismatched_calls = 0usize;
            let mut first_mismatch: Option<(usize, usize, i16, i16)> = None;
            for call_idx in 0..count {
                let base = 4 + call_idx * RECORD_BYTES;
                let block = &buf[base..base + BLOCK_BYTES];
                let state_bytes = &buf[base + BLOCK_BYTES..base + BLOCK_BYTES + STATE_BYTES];
                let expected = &buf[base + BLOCK_BYTES + STATE_BYTES..base + RECORD_BYTES];

                let mut nibbles = vec![0u8; SAMPLES_PER_CALL];
                unpack_nibbles_4bit(block, &mut nibbles);
                let mut state_l = state_from_bytes(&state_bytes[0..52]);
                let mut state_r = state_from_bytes(&state_bytes[52..104]);
                let mut ok = true;
                for (i, pair) in nibbles.chunks_exact(2).enumerate() {
                    let l = decode_nibble_4bit(&mut state_l, Nibble::new(pair[0]));
                    let r = decode_nibble_4bit(&mut state_r, Nibble::new(pair[1]));
                    let want_l = i16::from_le_bytes([expected[4 * i], expected[4 * i + 1]]);
                    let want_r = i16::from_le_bytes([expected[4 * i + 2], expected[4 * i + 3]]);
                    if l != want_l {
                        if first_mismatch.is_none() {
                            first_mismatch = Some((call_idx, 2 * i, l, want_l));
                        }
                        ok = false;
                    }
                    if r != want_r {
                        if first_mismatch.is_none() {
                            first_mismatch = Some((call_idx, 2 * i + 1, r, want_r));
                        }
                        ok = false;
                    }
                }
                if !ok {
                    mismatched_calls += 1;
                }
            }
            assert_eq!(
                mismatched_calls, 0,
                "{mismatched_calls}/{count} stereo calls mismatched; first diff: {first_mismatch:?}"
            );
        }
    }
}

pub mod ss2 {
    use super::codec3;

    /// Size of the per-bank wrapper.
    pub const WRAPPER_BYTES: usize = 0x2C;

    /// `+0x08` field of the wrapper; total byte size of this bank
    /// including wrapper.
    pub const BANK_SIZE_OFFSET: usize = 0x08;

    /// Cycle size in bytes (= wrapper `+0x10`).
    pub const CYCLE_BYTES: usize = 0x450;

    /// Per-voice header size at the start of each voice's region in
    /// cycle 0. Matches the standalone `.LS2` / `.SS2` layout: 28-byte
    /// codec3 header + 4 zero bytes + 36-byte PCM lookahead = 0x44.
    pub const VOICE_HEADER_BYTES: usize = 0x44;

    /// Number of voices per bank. Established empirically (3 distinct
    /// codec state-struct pointers in plugin trace, each producing a
    /// 1440-frame block per round-robin pass through `sub_198600`).
    pub const VOICES_PER_BANK: usize = 3;

    /// Voice 0/1/2 region sizes within cycle 0 (header + ADPCM).
    /// Sum = 0x16A + 0x169 + 0x169 = 0x43C; cycle 0 trailer fills
    /// to `CYCLE_BYTES`.
    const CYCLE0_VOICE_REGION_SIZES: [usize; VOICES_PER_BANK] = [0x16A, 0x169, 0x169];

    /// Cycle-0 byte offset of each voice's header (and ADPCM start
    /// at +VOICE_HEADER_BYTES).
    fn cycle0_voice_header_offset(voice: usize) -> usize {
        WRAPPER_BYTES + CYCLE0_VOICE_REGION_SIZES.iter().take(voice).sum::<usize>()
    }

    /// One parsed bank. Holds a slice into the bank's bytes plus the
    /// info needed to de-interleave any of the three voices. Use
    /// `voice_stream(idx)` to materialize voice `idx`'s logical
    /// codec_id=3 stream (header + ADPCM).
    #[derive(Clone, Debug)]
    pub struct Bank<'a> {
        pub index: usize,
        /// Absolute file offset of the wrapper.
        pub offset: usize,
        /// Total bank length (wrapper + cycles), from `+0x08`.
        pub bank_size: usize,
        /// Bytes of this bank, including the 0x2C wrapper.
        pub bytes: &'a [u8],
    }

    impl<'a> Bank<'a> {
        /// Reconstruct one voice's logical codec_id=3 stream by
        /// de-interleaving its chunks across all cycles. Returns
        /// `[28-byte codec3 header][raw ADPCM nibbles]` ready for
        /// `codec3::parse_header` / `codec3::decode_file`.
        pub fn voice_stream(&self, voice: usize) -> Vec<u8> {
            assert!(voice < VOICES_PER_BANK);
            let mut out = Vec::with_capacity(self.bank_size / VOICES_PER_BANK);

            // Header: 28 bytes from voice's cycle-0 header region.
            let h_start = cycle0_voice_header_offset(voice);
            out.extend_from_slice(&self.bytes[h_start..h_start + codec3::HEADER_BYTES]);

            // Cycle 0 ADPCM (right after the voice header, runs to end
            // of voice's cycle-0 region).
            let voice_size = CYCLE0_VOICE_REGION_SIZES[voice];
            let adpcm_start = h_start + VOICE_HEADER_BYTES;
            let adpcm_end = h_start + voice_size;
            out.extend_from_slice(&self.bytes[adpcm_start..adpcm_end]);

            // Cycles 1..N -- each voice gets 361 bytes per cycle, with
            // the bonus +1 rotating: cycle k -> voice (k % 3) gets +1.
            let mut cycle_idx: usize = 1;
            loop {
                let cycle_start = WRAPPER_BYTES + cycle_idx * CYCLE_BYTES;
                if cycle_start >= self.bytes.len() {
                    break;
                }
                let bonus_voice = cycle_idx % VOICES_PER_BANK;
                let mut sizes = [361usize; VOICES_PER_BANK];
                sizes[bonus_voice] += 1;
                let off_within_cycle: usize = sizes.iter().take(voice).sum();
                let chunk_start = cycle_start + off_within_cycle;
                let chunk_end = chunk_start + sizes[voice];
                if chunk_end > self.bytes.len() {
                    // Partial final chunk.
                    if chunk_start < self.bytes.len() {
                        out.extend_from_slice(&self.bytes[chunk_start..]);
                    }
                    break;
                }
                out.extend_from_slice(&self.bytes[chunk_start..chunk_end]);
                cycle_idx += 1;
            }
            out
        }
    }

    /// Iterate a multi-bank `.SS2` container. Returns one entry per
    /// bank in file order. Bails out with an error if a wrapper's
    /// signature doesn't match (`+0x00 != 2` or `+0x04 != 3`), or if
    /// the declared bank size walks past EOF.
    pub fn list(file_bytes: &[u8]) -> Result<Vec<Bank<'_>>, &'static str> {
        let mut out = Vec::new();
        let mut cursor = 0usize;
        while cursor < file_bytes.len() {
            if file_bytes.len() - cursor < WRAPPER_BYTES {
                return Err("trailing bytes shorter than a wrapper");
            }
            let magic_a = u32::from_le_bytes(file_bytes[cursor..cursor + 4].try_into().unwrap());
            let codec_id =
                u32::from_le_bytes(file_bytes[cursor + 4..cursor + 8].try_into().unwrap());
            if magic_a != 2 || codec_id != 3 {
                return Err("bank wrapper signature mismatch");
            }
            let bank_size = u32::from_le_bytes(
                file_bytes[cursor + BANK_SIZE_OFFSET..cursor + BANK_SIZE_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            // Smallest valid bank: wrapper + cycle 0 with all voice
            // headers + at least the cycle-0 trailer.
            let min_bank = WRAPPER_BYTES + CYCLE_BYTES;
            if bank_size < min_bank || cursor + bank_size > file_bytes.len() {
                return Err("bank size walks past EOF or is too small for the cycle layout");
            }
            let bytes = &file_bytes[cursor..cursor + bank_size];
            out.push(Bank {
                index: out.len(),
                offset: cursor,
                bank_size,
                bytes,
            });
            cursor += bank_size;
        }
        Ok(out)
    }

    /// Detect whether `file_bytes` is a wrapped multi-bank container
    /// or a single-stream codec_id=3 file. Returns `true` if the
    /// first 8 bytes match the wrapper signature.
    pub fn is_multibank(file_bytes: &[u8]) -> bool {
        file_bytes.len() >= 8
            && u32::from_le_bytes(file_bytes[0..4].try_into().unwrap()) == 2
            && u32::from_le_bytes(file_bytes[4..8].try_into().unwrap()) == 3
    }

    /// Decode one voice of a bank. Convenience: builds the voice's
    /// logical codec_id=3 stream via `voice_stream` and runs
    /// `codec3::decode_file` on it.
    pub fn decode_voice(bank: &Bank<'_>, voice: usize) -> Result<Vec<i16>, &'static str> {
        let stream = bank.voice_stream(voice);
        codec3::decode_file(&stream)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rejects_signature_mismatch() {
            let mut bytes = vec![0u8; WRAPPER_BYTES + CYCLE_BYTES];
            bytes[0] = 1; // magic_a wrong
            assert!(list(&bytes).is_err());
        }

        #[test]
        fn rejects_bank_size_past_eof() {
            let mut bytes = vec![0u8; WRAPPER_BYTES + CYCLE_BYTES];
            bytes[0..4].copy_from_slice(&2u32.to_le_bytes());
            bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&0xFFFFFFu32.to_le_bytes());
            assert!(list(&bytes).is_err());
        }

        #[test]
        fn voice_stream_starts_with_voice_header() {
            // One bank with cycle 0 only. Mark each voice's header
            // start with a unique codec_id=3 byte (0x03) so we can
            // confirm `voice_stream` extracts the right region.
            let bank_size = WRAPPER_BYTES + CYCLE_BYTES;
            let mut bytes = vec![0u8; bank_size];
            bytes[0..4].copy_from_slice(&2u32.to_le_bytes());
            bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&(bank_size as u32).to_le_bytes());
            // Voice 0 header
            let v0 = WRAPPER_BYTES;
            bytes[v0] = 0x03;
            bytes[v0 + 1] = 0xA0;
            // Voice 1 header
            let v1 = v0 + CYCLE0_VOICE_REGION_SIZES[0];
            bytes[v1] = 0x03;
            bytes[v1 + 1] = 0xA1;
            // Voice 2 header
            let v2 = v1 + CYCLE0_VOICE_REGION_SIZES[1];
            bytes[v2] = 0x03;
            bytes[v2 + 1] = 0xA2;

            let banks = list(&bytes).expect("parse");
            assert_eq!(banks.len(), 1);
            let b = &banks[0];
            for (i, expected_marker) in [0xA0, 0xA1, 0xA2].iter().enumerate() {
                let s = b.voice_stream(i);
                assert!(s.len() >= 2, "voice {i} stream too short: {}", s.len());
                assert_eq!(s[0], 0x03, "voice {i} should start with codec3 byte");
                assert_eq!(s[1], *expected_marker, "voice {i} marker mismatch");
            }
        }

        fn build_bank(bank_size: usize, voice_markers: [u8; VOICES_PER_BANK]) -> Vec<u8> {
            let mut bytes = vec![0u8; bank_size];
            bytes[0..4].copy_from_slice(&2u32.to_le_bytes());
            bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&(bank_size as u32).to_le_bytes());
            let mut off = WRAPPER_BYTES;
            for (voice, marker) in voice_markers.iter().enumerate() {
                bytes[off] = 0x03;
                bytes[off + 1] = *marker;
                off += CYCLE0_VOICE_REGION_SIZES[voice];
            }
            bytes
        }

        /// `is_multibank` must accept files whose first 8 bytes are the
        /// wrapper signature and reject codec_id=3 / codec_id=8 / random
        /// data so the dispatcher in bin.rs picks the right decoder.
        #[test]
        fn is_multibank_signature_detect() {
            assert!(is_multibank(&[2, 0, 0, 0, 3, 0, 0, 0]));
            assert!(!is_multibank(&[3, 0, 0, 0, 0, 0, 0, 0])); // codec_id=3 directly
            assert!(!is_multibank(&[8, 0, 0, 0, 0, 0, 0, 0])); // codec_id=8
            assert!(!is_multibank(&[2, 0, 0, 0])); // too short
            assert!(!is_multibank(&[])); // empty
        }

        /// Two consecutive banks must round-trip through `list`. The
        /// per-bank `offset` field is load-bearing for any extractor that
        /// indexes back into the source file.
        #[test]
        fn list_walks_multiple_banks() {
            let bank_size = WRAPPER_BYTES + CYCLE_BYTES;
            let bank0 = build_bank(bank_size, [0xA0, 0xA1, 0xA2]);
            let bank1 = build_bank(bank_size, [0xB0, 0xB1, 0xB2]);
            let mut file = Vec::new();
            file.extend_from_slice(&bank0);
            file.extend_from_slice(&bank1);

            let banks = list(&file).expect("parse");
            assert_eq!(banks.len(), 2);
            assert_eq!(banks[0].offset, 0);
            assert_eq!(banks[0].bank_size, bank_size);
            assert_eq!(banks[1].offset, bank_size);
            assert_eq!(banks[1].bank_size, bank_size);

            for (b, expected) in banks.iter().zip([[0xA0, 0xA1, 0xA2], [0xB0, 0xB1, 0xB2]]) {
                for (v, marker) in expected.iter().enumerate() {
                    let s = b.voice_stream(v);
                    assert_eq!(s[1], *marker);
                }
            }
        }

        /// Voice 0 of cycle 0 holds `0x16A - 0x44 = 294` ADPCM bytes; cycles
        /// `>= 1` give 361 bytes per voice plus a `+1` bonus that rotates
        /// `cycle_idx % VOICES_PER_BANK`. Verify the de-interleave assembles
        /// the right slices in the right order for cycles 1 and 2.
        #[test]
        fn voice_stream_assembles_rotation_correctly() {
            let bank_size = WRAPPER_BYTES + 3 * CYCLE_BYTES;
            let mut bytes = build_bank(bank_size, [0xA0, 0xA1, 0xA2]);

            // Stamp a unique byte at the START of each (cycle, voice)
            // chunk so we can identify it after de-interleave.
            // Cycle 0 voice 0 starts at WRAPPER_BYTES + VOICE_HEADER_BYTES.
            let c0v0_adpcm = WRAPPER_BYTES + VOICE_HEADER_BYTES;
            bytes[c0v0_adpcm] = 0xC0;

            // Cycle 1 (bonus rotates to voice 1). Voice 0 gets exactly 361
            // bytes starting at the cycle base.
            let c1 = WRAPPER_BYTES + CYCLE_BYTES;
            bytes[c1] = 0xC1; // voice 0
            bytes[c1 + 361] = 0xD1; // voice 1
            bytes[c1 + 361 + 362] = 0xE1; // voice 2

            // Cycle 2 (bonus rotates to voice 2). Voice 0=361, voice 1=361,
            // voice 2=362.
            let c2 = WRAPPER_BYTES + 2 * CYCLE_BYTES;
            bytes[c2] = 0xC2; // voice 0
            bytes[c2 + 361] = 0xD2; // voice 1
            bytes[c2 + 361 + 361] = 0xE2; // voice 2

            let banks = list(&bytes).expect("parse");
            let b = &banks[0];

            // Voice 0 stream: 28-byte header + 0x126 (294) bytes from cycle 0
            // + 361 + 361 = 1044 bytes total.
            let v0 = b.voice_stream(0);
            assert_eq!(v0.len(), codec3::HEADER_BYTES + 294 + 361 + 361);
            assert_eq!(v0[codec3::HEADER_BYTES], 0xC0); // cycle 0 start
            assert_eq!(v0[codec3::HEADER_BYTES + 294], 0xC1); // cycle 1 start
            assert_eq!(v0[codec3::HEADER_BYTES + 294 + 361], 0xC2); // cycle 2 start

            // Voice 1 stream: header + (0x169 - 0x44) + 362 + 361.
            let v1 = b.voice_stream(1);
            let v1_c0_len = CYCLE0_VOICE_REGION_SIZES[1] - VOICE_HEADER_BYTES;
            assert_eq!(v1.len(), codec3::HEADER_BYTES + v1_c0_len + 362 + 361);
            assert_eq!(v1[codec3::HEADER_BYTES + v1_c0_len], 0xD1);
            assert_eq!(v1[codec3::HEADER_BYTES + v1_c0_len + 362], 0xD2);

            // Voice 2 stream: header + (0x169 - 0x44) + 361 + 362.
            let v2 = b.voice_stream(2);
            let v2_c0_len = CYCLE0_VOICE_REGION_SIZES[2] - VOICE_HEADER_BYTES;
            assert_eq!(v2.len(), codec3::HEADER_BYTES + v2_c0_len + 361 + 362);
            assert_eq!(v2[codec3::HEADER_BYTES + v2_c0_len], 0xE1);
            assert_eq!(v2[codec3::HEADER_BYTES + v2_c0_len + 361], 0xE2);
        }

        /// End-to-end through `decode_voice`: stamp a valid codec3 header
        /// (mode=0 / planar mono) at voice 0's cycle-0 region and check
        /// `codec3::decode_file` actually accepts the assembled stream.
        /// The first sample of the all-zero ADPCM body must equal the
        /// header predictor seed = 0 (the parser zero-seeds state per the
        /// QEMU plugin trace; see `codec3::parse_header`).
        #[test]
        fn decode_voice_round_trip_with_valid_header() {
            let bank_size = WRAPPER_BYTES + CYCLE_BYTES;
            let mut bytes = build_bank(bank_size, [0x00, 0x00, 0x00]);
            // Place a valid codec3 header at voice 0's cycle-0 region.
            // version=3, mode=0 (planar mono). All other bytes 0.
            let v0 = WRAPPER_BYTES;
            bytes[v0] = 0x03; // version
            bytes[v0 + 12] = 0x00; // mode

            let banks = list(&bytes).expect("parse");
            let pcm = decode_voice(&banks[0], 0).expect("decode");
            // Voice 0 ADPCM payload is 294 bytes of zeros, decoded as
            // 588 mono samples; predictor seeded to 0 means the entire
            // run stays at 0 (each nibble=0 with step_index=0 yields
            // delta=0).
            assert_eq!(pcm.len(), 294 * 2);
            assert!(pcm.iter().all(|&s| s == 0));
        }
    }
}

/// `.SM2` (Maps BigFile) outer directory parsing. Format is reversed
/// from `sub_17e250` (loads/validates the directory) and `sub_17e3f0`
/// (linear search by map name). The format:
///
///   offset 0x00  u32  version              -- must be 7 in SC1 NTSC
///   offset 0x04  u32  record_table_offset  -- typically 0x0c
///   offset 0x08  u32  record_count         -- number of records
///   offset record_table_offset:
///     repeat record_count times {
///       u32  reserved_0  (always 0 on disk; runtime field)
///       u32  reserved_1  (always 0 on disk; runtime field)
///       u32  data_offset -- absolute file offset of this map's data
///       u32  data_size   -- bytes to read at `data_offset`
///       char name[0x20]  -- null-terminated map name (e.g. "0_0_2_Training")
///     }  // 0x30 bytes per record
///
/// `sub_17e470` allocates `data_size` bytes, seeks to `data_offset`,
/// reads `data_size` bytes -- that's the per-map descriptor blob,
/// itself a fixup-relative pointer-graph. See AUDIO.md for the next
/// layer.
pub mod sm2 {
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::{
        self,
    };

    pub const VERSION: u32 = 7;
    pub const RECORD_BYTES: usize = 0x30;
    pub const NAME_OFFSET_IN_RECORD: usize = 0x10;
    pub const NAME_BYTES: usize = RECORD_BYTES - NAME_OFFSET_IN_RECORD;

    #[derive(Clone, Debug)]
    pub struct Record {
        pub data_offset: u32,
        pub data_size: u32,
        pub name: String,
    }

    /// One sound entry from a map's audio table. Offset is absolute
    /// into the source `.SM2` file. `length` is derived from the
    /// difference between consecutive entries' offsets -- the last
    /// entry's length is the gap from its offset to the start of the
    /// next map's data section.
    ///
    /// `sample_rate` and `channels` are pulled from the metadata
    /// record at `array_b[seq_id]` (120-byte struct containing the
    /// source `.wav` name, sample count, byte length, sample rate,
    /// and average bytes-per-second).
    ///
    /// `is_ls2_redirect` is true when the array_b entry has an `LS2`
    /// tag in the last 3 bytes of its 16-byte name field. The bytes
    /// at `file_offset` are placeholder/dead -- the engine plays the
    /// actual sound from the corresponding `.LS2` file via the
    /// codec_id=3 streaming path, not via SetBufferData on the
    /// MAPS.LM2 bytes. Verified by QEMU plugin trace: zero overlap
    /// between SM2 SFX buffers (859 unique submits) and streaming
    /// buffers (16 unique with SetFrequency calls).
    #[derive(Clone, Debug)]
    pub struct SoundEntry {
        pub seq_id: super::units::SeqId,
        pub file_offset: u64,
        pub length: u64,
        pub sample_rate: super::units::SampleRate,
        pub channels: u16,
        pub source_name: String,
        pub is_ls2_redirect: bool,
    }

    const ARRAY_B_ENTRY_SIZE: usize = 120;
    const ARRAY_B_OFFSET_SAMPLE_RATE: usize = 0x44;
    const ARRAY_B_OFFSET_AVG_BYTES_PER_SEC: usize = 0x40;
    /// Channel count lives in the HIGH word of the u32 at `+0x48`
    /// (i.e. read u16 at `+0x4a`). The low word is `0x0010` for all
    /// observed entries -- likely a fixed format-flags constant.
    /// Verified: FN20IS_1 has `0x00020010` here (stereo, 44.1kHz --
    /// matches its 72-byte block alignment) while every mono entry
    /// has `0x00010010`.
    const ARRAY_B_OFFSET_CHANNELS: usize = 0x4a;
    const ARRAY_B_OFFSET_NAME: usize = 0x50;
    const ARRAY_B_NAME_BYTES: usize = 16;

    /// Per-map `array_a`: 88-byte records keyed by seq_id, used to
    /// look up the playback-rate ratio for LS2-tagged sub_a entries.
    /// Each record holds a seq_id at `+0x08` and a 16:16 fixed-point
    /// rate ratio at `+0x10` (e.g. `0x10000` = 1.0 = play at the
    /// `array_b[+0x44]` rate; `0xAADA` = 0.6674 = play at ~10.7 kHz
    /// for a 16 kHz nominal entry).
    const ARRAY_A_ENTRY_SIZE: usize = 88;
    const ARRAY_A_OFFSET_SEQ_ID: usize = 0x08;
    const ARRAY_A_OFFSET_RATE_RATIO: usize = 0x10;
    /// Denominator for the 16:16 fixed-point rate ratio at
    /// `array_a[+0x10]`. `value / RATE_RATIO_UNIT` yields the
    /// multiplier applied to `array_b[+0x44]` for the actual
    /// playback rate.
    const RATE_RATIO_UNIT: u64 = 0x10000;

    /// Parse a map's per-sound table. Format derived from
    /// `sub_17e470`'s post-load fixup loop and confirmed against
    /// QEMU plugin SetBufferData traces:
    ///
    ///   descriptor[+0x14] = relative offset of `array_c`
    ///   descriptor[+0x18] = count of `array_c` (always observed = 1)
    ///   array_c[0] is a 20-byte header with two fixup-relative
    ///   pointers at +0x04 (first sub-array) and +0x0c (second
    ///   sub-array), each followed by their own count.
    ///
    /// The first sub-array is the per-sound table: 8 bytes per
    /// entry, `(seq_id_u32, audio_rel_offset_u32)`. The audio bytes
    /// for a map start immediately after the descriptor blob in the
    /// source `.SM2` file (i.e. at `record.data_offset +
    /// record.data_size`); each entry's `audio_rel_offset` is
    /// relative to that audio base, and the length is implicit:
    /// `next_entry.audio_rel_offset - this_entry.audio_rel_offset`.
    /// Container kind. Both `.SM2` (SFX/ambient) and `.LM2`
    /// (language-specific dialog) share the v7 archive header and the
    /// descriptor blob layout, but differ in how `array_a`'s rate
    /// ratio applies: SM2 only scales LS2-tagged entries (rest play at
    /// nominal rate), LM2 scales every entry. The `kind` lets the
    /// parser pick the right rule.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Kind {
        Sm2,
        Lm2,
    }

    pub fn parse_sound_table(
        descriptor: &[u8],
        record: &Record,
        next_record_offset: Option<u32>,
    ) -> io::Result<Vec<SoundEntry>> {
        parse_sound_table_for(descriptor, record, next_record_offset, Kind::Sm2)
    }

    pub fn parse_sound_table_for(
        descriptor: &[u8],
        record: &Record,
        next_record_offset: Option<u32>,
        kind: Kind,
    ) -> io::Result<Vec<SoundEntry>> {
        if descriptor.len() < 0x24 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "descriptor too small for top header",
            ));
        }
        let array_a_off = u32::from_le_bytes(descriptor[0x04..0x08].try_into().unwrap()) as usize;
        let array_a_cnt = u32::from_le_bytes(descriptor[0x08..0x0c].try_into().unwrap()) as usize;
        let array_b_off = u32::from_le_bytes(descriptor[0x0c..0x10].try_into().unwrap()) as usize;
        let array_b_cnt = u32::from_le_bytes(descriptor[0x10..0x14].try_into().unwrap());
        let array_c_off = u32::from_le_bytes(descriptor[0x14..0x18].try_into().unwrap()) as usize;
        let array_c_cnt = u32::from_le_bytes(descriptor[0x18..0x1c].try_into().unwrap());

        // Build a sorted (seq_id, rate_ratio) view of array_a so we can
        // binary-search for an LS2-tagged sub_a entry's nearest array_a
        // entry by seq. Verified rule (5_1_2_PresidentialPalace, all
        // 169 entries): for each LS2-tagged sub_a entry, the engine's
        // playback rate is `array_b[+0x44] * ratio / RATE_RATIO_UNIT`,
        // where `ratio` is the +0x10 field of the array_a entry whose
        // seq_id is the largest <= the sub_a seq. Non-LS2 entries play
        // at `array_b[+0x44]` directly.
        //
        // Some maps (e.g. 4_2_1_Abattoir) have array_a entries with
        // ratio=0 and/or duplicate seq_ids in non-monotonic order.
        // The zero-ratio entries appear to be sentinel/placeholder
        // slots and would crash the WAV writer if applied; skip them.
        // For duplicates, the highest-index entry with non-zero ratio
        // wins after the stable sort.
        let mut array_a_by_seq: Vec<(u32, u32)> = Vec::with_capacity(array_a_cnt);
        if array_a_off + array_a_cnt * ARRAY_A_ENTRY_SIZE <= descriptor.len() {
            for j in 0..array_a_cnt {
                let off = array_a_off + j * ARRAY_A_ENTRY_SIZE;
                let s = u32::from_le_bytes(
                    descriptor[off + ARRAY_A_OFFSET_SEQ_ID..off + ARRAY_A_OFFSET_SEQ_ID + 4]
                        .try_into()
                        .unwrap(),
                );
                let r = u32::from_le_bytes(
                    descriptor
                        [off + ARRAY_A_OFFSET_RATE_RATIO..off + ARRAY_A_OFFSET_RATE_RATIO + 4]
                        .try_into()
                        .unwrap(),
                );
                if r != 0 {
                    array_a_by_seq.push((s, r));
                }
            }
            array_a_by_seq.sort_by_key(|&(s, _)| s);
        }
        if array_c_cnt != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected array_c count = 1, got {array_c_cnt}"),
            ));
        }
        if array_c_off + 20 > descriptor.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "array_c entry out of bounds",
            ));
        }
        let entry = &descriptor[array_c_off..array_c_off + 20];
        let sub_a_rel = u32::from_le_bytes(entry[0x04..0x08].try_into().unwrap()) as usize;
        let sub_a_cnt = u32::from_le_bytes(entry[0x08..0x0c].try_into().unwrap()) as usize;

        let sub_a_off = array_c_off + sub_a_rel;
        let table_bytes = sub_a_cnt * 8;
        if sub_a_off + table_bytes > descriptor.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sub_a table out of bounds",
            ));
        }

        let audio_base = record.data_offset as u64 + record.data_size as u64;
        let audio_end = next_record_offset.map(|n| n as u64).unwrap_or(u64::MAX);

        let mut entries: Vec<(u32, u32)> = Vec::with_capacity(sub_a_cnt);
        for i in 0..sub_a_cnt {
            let off = sub_a_off + i * 8;
            let seq_id = u32::from_le_bytes(descriptor[off..off + 4].try_into().unwrap());
            let rel = u32::from_le_bytes(descriptor[off + 4..off + 8].try_into().unwrap());
            entries.push((seq_id, rel));
        }

        let mut out = Vec::with_capacity(sub_a_cnt);
        for (i, &(seq_id, rel)) in entries.iter().enumerate() {
            let file_offset = audio_base + rel as u64;
            let next_off = if i + 1 < entries.len() {
                audio_base + entries[i + 1].1 as u64
            } else {
                audio_end
            };
            let length = next_off.saturating_sub(file_offset);

            // Look up per-sound metadata in array_b[seq_id & 0xFFFFFF].
            // The high byte of `seq_id` is a TYPE tag (e.g. 0x40 for
            // typical SFX); the low 24 bits index `array_b`. Without
            // the mask, seq_ids like 0x40000001 are huge u32s that
            // never match `seq_id < array_b_cnt`, and the sound falls
            // back to a hardcoded 22050 Hz -- wrong rate, audible as
            // ~1.38x playback speed for the typical 16000 Hz SFX.
            //
            // Each array_b entry is 120 bytes; +0x44 = sample_rate,
            // +0x40 = avg bytes-per-sec, +0x4a = channel count (u16
            // high word of the u32 at +0x48), +0x50 = 16-byte source
            // `.wav` name.
            //
            // Entries whose name field ends in "LS2" (last 3 bytes
            // of the 16-byte name buffer) carry an LS2 tag. Two
            // encodings observed:
            //   "Music_Common.LS2"          -- the LS2 filename itself
            //   "EFOLOU_1.wav\0LS2"         -- original .wav name + tag
            // Both share `name_bytes[13..16] == b"LS2"`.
            //
            // QEMU trace evidence (5_1_2_PresidentialPalace, 1024
            // SetBufferData submits): 12/12 non-LS2 entries match
            // submits at submit_len == sub_a length; 112/157 LS2
            // entries also match. So the bytes ARE played from
            // MAPS.LM2 for most LS2-tagged entries.
            //
            // Per static analysis of `sub_1885a0` (SFX buffer
            // creator), playback rate comes from this `+0x44`
            // field via the runtime sound struct (`+0x2c`). The
            // engine has a per-play SetFrequency override path
            // (`sub_1888a0`) but the trace shows it only fires
            // at music/stream rates (36k/48k/22k/44k), never at
            // sub-16k for SFX. So per the binary, all entries here
            // should play at +0x44 Hz.
            out.push(build_sound_entry(
                descriptor,
                array_b_off,
                array_b_cnt,
                &array_a_by_seq,
                seq_id,
                file_offset,
                length,
                kind,
            ));
        }
        Ok(out)
    }

    /// Build one `SoundEntry` by looking up `array_b[seq_id & 0xFFFFFF]`
    /// and applying SM2/LM2 rate scaling. The high byte of `seq_id` is a
    /// TYPE tag (e.g. 0x40 for typical SFX); the low 24 bits index
    /// `array_b`. Without the mask, seq_ids like 0x40000001 are huge
    /// u32s that never match `seq_id < array_b_cnt`, and the sound
    /// falls back to a hardcoded 22050 Hz -- wrong rate, audible as
    /// ~1.38x playback speed for the typical 16000 Hz SFX.
    ///
    /// Each array_b entry is 120 bytes; `+0x44` = sample_rate,
    /// `+0x40` = avg bytes-per-sec, `+0x4a` = channel count (u16 high
    /// word of the u32 at `+0x48`), `+0x50` = 16-byte source `.wav`
    /// name. Entries whose name field ends in "LS2" (bytes 13..16)
    /// carry an LS2 tag. Two encodings observed:
    ///   "Music_Common.LS2"          -- the LS2 filename itself
    ///   "EFOLOU_1.wav\0LS2"         -- original .wav name + tag
    ///
    /// Rate scaling rule: SM2 only applies array_a's ratio to
    /// LS2-tagged entries (verified against the engine on
    /// 5_1_2_PresidentialPalace). LM2 (language-specific dialog
    /// archive) applies the ratio to every entry -- its array_b names
    /// omit the `LS2` suffix (they store the bare `.wav` filename), so
    /// the tag check would never fire. Empirically required:
    /// `ENGLISH/MAPS.LM2` 0_0_3 entries from `seq=0x4000007e` onward
    /// play at 0.6674x nominal per array_a, otherwise dialog plays too
    /// fast.
    #[allow(clippy::too_many_arguments)]
    fn build_sound_entry(
        descriptor: &[u8],
        array_b_off: usize,
        array_b_cnt: u32,
        array_a_by_seq: &[(u32, u32)],
        seq_id: u32,
        file_offset: u64,
        length: u64,
        kind: Kind,
    ) -> SoundEntry {
        // Engine fallback for out-of-range / truncated descriptor:
        // 22050 Hz mono, no source name, no LS2 tag. Acts as the base
        // for struct-update on the success path -- the lookup only
        // overrides the four array_b-derived fields.
        let fallback = SoundEntry {
            seq_id: super::units::SeqId::new(seq_id),
            file_offset,
            length,
            sample_rate: super::units::SampleRate::new(22050),
            channels: 1,
            source_name: String::new(),
            is_ls2_redirect: false,
        };

        let array_b_idx = (seq_id & 0x00FF_FFFF) as usize;
        if array_b_idx >= array_b_cnt as usize {
            return fallback;
        }
        let entry_off = array_b_off + array_b_idx * ARRAY_B_ENTRY_SIZE;
        if entry_off + ARRAY_B_ENTRY_SIZE > descriptor.len() {
            return fallback;
        }
        let array_b_entry = &descriptor[entry_off..entry_off + ARRAY_B_ENTRY_SIZE];
        let nominal_sr = u32::from_le_bytes(
            array_b_entry[ARRAY_B_OFFSET_SAMPLE_RATE..ARRAY_B_OFFSET_SAMPLE_RATE + 4]
                .try_into()
                .unwrap(),
        );
        let ch = u16::from_le_bytes(
            array_b_entry[ARRAY_B_OFFSET_CHANNELS..ARRAY_B_OFFSET_CHANNELS + 2]
                .try_into()
                .unwrap(),
        );
        let channels = if ch == 1 || ch == 2 { ch } else { 1 };
        let name_bytes =
            &array_b_entry[ARRAY_B_OFFSET_NAME..ARRAY_B_OFFSET_NAME + ARRAY_B_NAME_BYTES];
        let is_ls2_redirect = &name_bytes[13..16] == b"LS2";
        let name_end = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(ARRAY_B_NAME_BYTES);
        let source_name = String::from_utf8_lossy(&name_bytes[..name_end]).into_owned();

        let scale = is_ls2_redirect || kind == Kind::Lm2;
        let nominal = super::units::SampleRate::new(nominal_sr);
        let sample_rate = if scale {
            let ratio_raw = array_a_by_seq
                .partition_point(|&(s, _)| s <= seq_id)
                .checked_sub(1)
                .map(|i| array_a_by_seq[i].1)
                .unwrap_or(super::units::RateRatio::UNIT.raw());
            super::units::RateRatio::new(ratio_raw).apply(nominal)
        } else {
            nominal
        };

        SoundEntry {
            sample_rate,
            channels,
            source_name,
            is_ls2_redirect,
            ..fallback
        }
    }

    /// Parse the outer directory of a `.SM2` (or compatible) file.
    pub fn read_directory<R: Read + Seek>(reader: &mut R) -> io::Result<Vec<Record>> {
        reader.seek(SeekFrom::Start(0))?;
        let mut header = [0u8; 12];
        reader.read_exact(&mut header)?;
        let version = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected .SM2 version {VERSION}, got {version}"),
            ));
        }
        let table_offset = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let record_count = u32::from_le_bytes(header[8..12].try_into().unwrap());

        reader.seek(SeekFrom::Start(table_offset as u64))?;
        let mut records = Vec::with_capacity(record_count as usize);
        let mut buf = [0u8; RECORD_BYTES];
        for _ in 0..record_count {
            reader.read_exact(&mut buf)?;
            let data_offset = u32::from_le_bytes(buf[0x08..0x0c].try_into().unwrap());
            let data_size = u32::from_le_bytes(buf[0x0c..0x10].try_into().unwrap());
            let name_bytes = &buf[NAME_OFFSET_IN_RECORD..];
            let name_end = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(NAME_BYTES);
            let name = String::from_utf8_lossy(&name_bytes[..name_end]).into_owned();
            records.push(Record {
                data_offset,
                data_size,
                name,
            });
        }
        Ok(records)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Build a synthetic descriptor blob laid out as the engine
        /// expects. `sub_a`: `(seq_id, audio_rel_offset)`. `array_b_entry`:
        /// `(avg_bps, sample_rate, channels, name_16)` where the 16-byte
        /// name field is taken verbatim (bytes 13..16 of `b"LS2"` mark
        /// LS2-redirected entries). `array_a`: `(seq_id, ratio)`.
        fn build_descriptor(
            sub_a: &[(u32, u32)],
            array_b: &[(u32, u32, u16, [u8; ARRAY_B_NAME_BYTES])],
            array_a: &[(u32, u32)],
        ) -> Vec<u8> {
            const HEADER_BYTES: usize = 0x24;
            const ARRAY_C_BYTES: usize = 20;
            let array_a_size = array_a.len() * ARRAY_A_ENTRY_SIZE;
            let array_b_size = array_b.len() * ARRAY_B_ENTRY_SIZE;
            let sub_a_size = sub_a.len() * 8;

            let array_a_off = HEADER_BYTES;
            let array_b_off = array_a_off + array_a_size;
            let array_c_off = array_b_off + array_b_size;
            let sub_a_off = array_c_off + ARRAY_C_BYTES;
            let total = sub_a_off + sub_a_size;

            let mut buf = vec![0u8; total];
            buf[0x04..0x08].copy_from_slice(&(array_a_off as u32).to_le_bytes());
            buf[0x08..0x0c].copy_from_slice(&(array_a.len() as u32).to_le_bytes());
            buf[0x0c..0x10].copy_from_slice(&(array_b_off as u32).to_le_bytes());
            buf[0x10..0x14].copy_from_slice(&(array_b.len() as u32).to_le_bytes());
            buf[0x14..0x18].copy_from_slice(&(array_c_off as u32).to_le_bytes());
            buf[0x18..0x1c].copy_from_slice(&1u32.to_le_bytes());

            for (i, &(seq, ratio)) in array_a.iter().enumerate() {
                let off = array_a_off + i * ARRAY_A_ENTRY_SIZE;
                buf[off + ARRAY_A_OFFSET_SEQ_ID..off + ARRAY_A_OFFSET_SEQ_ID + 4]
                    .copy_from_slice(&seq.to_le_bytes());
                buf[off + ARRAY_A_OFFSET_RATE_RATIO..off + ARRAY_A_OFFSET_RATE_RATIO + 4]
                    .copy_from_slice(&ratio.to_le_bytes());
            }

            for (i, &(avg, rate, ch, name)) in array_b.iter().enumerate() {
                let off = array_b_off + i * ARRAY_B_ENTRY_SIZE;
                buf[off + ARRAY_B_OFFSET_AVG_BYTES_PER_SEC
                    ..off + ARRAY_B_OFFSET_AVG_BYTES_PER_SEC + 4]
                    .copy_from_slice(&avg.to_le_bytes());
                buf[off + ARRAY_B_OFFSET_SAMPLE_RATE..off + ARRAY_B_OFFSET_SAMPLE_RATE + 4]
                    .copy_from_slice(&rate.to_le_bytes());
                // Channel count: u16 high word of the u32 at +0x48. Low
                // word is the engine's format-flag constant `0x0010` for
                // every observed entry; pin it so the test doesn't drift
                // if the parser ever starts validating it.
                let ch_u32 = ((ch as u32) << 16) | 0x0010;
                buf[off + 0x48..off + 0x4c].copy_from_slice(&ch_u32.to_le_bytes());
                buf[off + ARRAY_B_OFFSET_NAME..off + ARRAY_B_OFFSET_NAME + ARRAY_B_NAME_BYTES]
                    .copy_from_slice(&name);
            }

            let sub_a_rel = sub_a_off - array_c_off;
            buf[array_c_off + 0x04..array_c_off + 0x08]
                .copy_from_slice(&(sub_a_rel as u32).to_le_bytes());
            buf[array_c_off + 0x08..array_c_off + 0x0c]
                .copy_from_slice(&(sub_a.len() as u32).to_le_bytes());

            for (i, &(seq, rel)) in sub_a.iter().enumerate() {
                let off = sub_a_off + i * 8;
                buf[off..off + 4].copy_from_slice(&seq.to_le_bytes());
                buf[off + 4..off + 8].copy_from_slice(&rel.to_le_bytes());
            }
            buf
        }

        fn name_with_ls2(prefix: &[u8]) -> [u8; ARRAY_B_NAME_BYTES] {
            let mut out = [0u8; ARRAY_B_NAME_BYTES];
            let p = prefix.len().min(13);
            out[..p].copy_from_slice(&prefix[..p]);
            out[13..16].copy_from_slice(b"LS2");
            out
        }

        fn name_plain(prefix: &[u8]) -> [u8; ARRAY_B_NAME_BYTES] {
            let mut out = [0u8; ARRAY_B_NAME_BYTES];
            let p = prefix.len().min(16);
            out[..p].copy_from_slice(&prefix[..p]);
            out
        }

        fn record() -> Record {
            Record {
                data_offset: 0x1000,
                data_size: 0x200,
                name: "test_map".into(),
            }
        }

        /// Two sub_a entries: file_offset relative to audio_base, length
        /// is `next_offset - this_offset` for the first and
        /// `audio_end - last_offset` for the last. `0x40_00_00_01` /
        /// `0x40_00_00_02` exercise the seq-id type-tag mask.
        #[test]
        fn basic_two_entries_offsets_and_lengths() {
            // array_b indexed by seq & 0x00FFFFFF -> entries 1 and 2.
            let array_b = vec![
                (0, 0, 0, [0u8; ARRAY_B_NAME_BYTES]),
                (0, 22050, 1, name_plain(b"sfx_a.wav")),
                (0, 16000, 1, name_plain(b"sfx_b.wav")),
            ];
            let sub_a = vec![(0x4000_0001u32, 0u32), (0x4000_0002u32, 100u32)];
            let desc = build_descriptor(&sub_a, &array_b, &[]);
            let r = record();
            let audio_base = r.data_offset as u64 + r.data_size as u64;
            let next_record = Some(0x1500u32);

            let out = parse_sound_table_for(&desc, &r, next_record, Kind::Sm2).expect("parse");
            assert_eq!(out.len(), 2);
            assert_eq!(out[0].file_offset, audio_base);
            assert_eq!(out[0].length, 100);
            assert_eq!(out[0].sample_rate.hz(), 22050);
            assert_eq!(out[0].channels, 1);
            assert_eq!(out[0].source_name, "sfx_a.wav");
            assert!(!out[0].is_ls2_redirect);

            assert_eq!(out[1].file_offset, audio_base + 100);
            assert_eq!(out[1].length, 0x1500 - (audio_base + 100));
            assert_eq!(out[1].sample_rate.hz(), 16000);
        }

        /// SM2 only scales LS2-tagged entries; non-LS2 stays at the
        /// array_b nominal rate even when array_a has a matching ratio.
        #[test]
        fn sm2_non_ls2_ignores_array_a_ratio() {
            let array_b = vec![(0, 16000, 1, name_plain(b"plain.wav"))];
            let sub_a = vec![(0u32, 0u32)];
            // Force a non-unity ratio for the matching seq.
            let array_a = vec![(0u32, 0xAADAu32)]; // 0.6674x
            let desc = build_descriptor(&sub_a, &array_b, &array_a);
            let r = record();
            let out = parse_sound_table_for(&desc, &r, None, Kind::Sm2).expect("parse");
            assert_eq!(out[0].sample_rate.hz(), 16000);
            assert!(!out[0].is_ls2_redirect);
        }

        /// SM2 LS2-tagged entry gets scaled by the array_a ratio whose
        /// seq_id is the largest <= this entry's seq_id.
        #[test]
        fn sm2_ls2_entry_gets_array_a_scaling() {
            let array_b = vec![(0, 16000, 1, name_with_ls2(b"DIA_A"))];
            let sub_a = vec![(0u32, 0u32)];
            let array_a = vec![(0u32, 0xAADAu32)];
            let desc = build_descriptor(&sub_a, &array_b, &array_a);
            let r = record();
            let out = parse_sound_table_for(&desc, &r, None, Kind::Sm2).expect("parse");
            assert!(out[0].is_ls2_redirect);
            assert_eq!(
                out[0].sample_rate.hz(),
                (16000u64 * 0xAADA / 0x10000) as u32
            );
        }

        /// LM2 scales every entry regardless of LS2 tag (LM2 array_b
        /// names omit the suffix; rate scaling is applied unconditionally).
        #[test]
        fn lm2_scales_every_entry() {
            let array_b = vec![(0, 16000, 1, name_plain(b"dialog.wav"))];
            let sub_a = vec![(0u32, 0u32)];
            let array_a = vec![(0u32, 0xAADAu32)];
            let desc = build_descriptor(&sub_a, &array_b, &array_a);
            let r = record();
            let out = parse_sound_table_for(&desc, &r, None, Kind::Lm2).expect("parse");
            assert!(!out[0].is_ls2_redirect);
            assert_eq!(
                out[0].sample_rate.hz(),
                (16000u64 * 0xAADA / 0x10000) as u32
            );
        }

        /// array_a entries with `ratio == 0` are sentinel/placeholder
        /// slots and are skipped. A scaled entry whose seq matches only
        /// a zero-ratio slot must fall back to ratio = UNIT (nominal
        /// rate), not crash and not multiply by zero.
        #[test]
        fn lm2_zero_ratio_falls_back_to_unit() {
            let array_b = vec![(0, 16000, 1, name_plain(b"dialog.wav"))];
            let sub_a = vec![(0u32, 0u32)];
            let array_a = vec![(0u32, 0u32)]; // sentinel
            let desc = build_descriptor(&sub_a, &array_b, &array_a);
            let r = record();
            let out = parse_sound_table_for(&desc, &r, None, Kind::Lm2).expect("parse");
            assert_eq!(out[0].sample_rate.hz(), 16000);
        }

        /// Out-of-bounds seq_id (after the `& 0x00FFFFFF` mask still
        /// >= array_b_cnt) falls back to the hardcoded 22050 default.
        /// This guards the path that protected real maps before the
        /// type-tag mask was applied.
        #[test]
        fn out_of_range_seq_falls_back_to_default_rate() {
            // array_b_cnt = 1, but seq's low-24 = 0x10 -> beyond the array.
            let array_b = vec![(0, 16000, 1, name_plain(b"a.wav"))];
            let sub_a = vec![(0x4000_0010u32, 0u32)];
            let desc = build_descriptor(&sub_a, &array_b, &[]);
            let r = record();
            let out = parse_sound_table_for(&desc, &r, None, Kind::Sm2).expect("parse");
            assert_eq!(out[0].sample_rate.hz(), 22050);
            assert_eq!(out[0].source_name, "");
        }

        #[test]
        fn rejects_descriptor_shorter_than_header() {
            assert!(parse_sound_table_for(&[0u8; 0x20], &record(), None, Kind::Sm2).is_err());
        }

        #[test]
        fn rejects_array_c_count_not_one() {
            let array_b = vec![(0, 16000, 1, name_plain(b"a.wav"))];
            let sub_a = vec![(0u32, 0u32)];
            let mut desc = build_descriptor(&sub_a, &array_b, &[]);
            // Smash array_c count to 2.
            desc[0x18..0x1c].copy_from_slice(&2u32.to_le_bytes());
            assert!(parse_sound_table_for(&desc, &record(), None, Kind::Sm2).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity: a block of all-zero data with a non-zero predictor seed
    /// should produce 64 samples that drift from the seed by small
    /// per-step amounts. The first sample is decoded from nibble 0,
    /// which contributes `step >> 3 = 7 >> 3 = 0` at step_index 0,
    /// so the first sample equals the predictor seed exactly.
    #[test]
    fn mono_zero_data_first_sample_equals_predictor() {
        let mut block = [0u8; 36];
        // predictor = 1234, step_index = 0
        block[0] = 0xd2;
        block[1] = 0x04;
        block[2] = 0x00;
        let pcm = decode_block_mono(&block);
        assert_eq!(pcm[0], 1234);
    }

    /// Block size is 36 bytes per channel; check that decoding sizes
    /// match the constants we publish.
    #[test]
    fn mono_decode_lengths_track_input_blocks() {
        let mut buf = Vec::new();
        // Two blocks worth of zero data, predictor=0, step_index=0.
        buf.extend_from_slice(&[0u8; 36]);
        buf.extend_from_slice(&[0u8; 36]);
        let pcm = decode_mono(&buf);
        assert_eq!(pcm.len(), 2 * SAMPLES_PER_BLOCK);
    }

    /// Step-index out-of-range in the header (>88) is clamped on
    /// construction. Some encoders emit a sentinel like 0xff when the
    /// block is silent or invalid; we should still decode without
    /// panicking on a step-table OOB lookup.
    #[test]
    fn step_index_out_of_range_is_clamped() {
        let mut block = [0u8; 36];
        block[2] = 0xff;
        let _ = decode_block_mono(&block); // must not panic
    }

    /// A monotonic positive nibble sequence (all 0x07 = max positive
    /// magnitude) should walk the predictor upward and saturate at
    /// 32767 within ~64 samples.
    #[test]
    fn max_positive_nibbles_saturate_high() {
        let mut block = [0u8; 36];
        // predictor=0, step_index=0
        for i in 4..36 {
            block[i] = 0x77; // both nibbles = 7 (max positive)
        }
        let pcm = decode_block_mono(&block);
        assert_eq!(*pcm.last().unwrap(), 32767);
    }

    /// Symmetric to the above -- all 0x0f nibbles (max negative) must
    /// saturate at -32768.
    #[test]
    fn max_negative_nibbles_saturate_low() {
        let mut block = [0u8; 36];
        for i in 4..36 {
            block[i] = 0xff; // both nibbles = 15 (max negative)
        }
        let pcm = decode_block_mono(&block);
        assert_eq!(*pcm.last().unwrap(), -32768);
    }

    /// Stereo block: independent L/R headers with different predictors
    /// should produce independent first samples per channel.
    #[test]
    fn stereo_first_samples_track_per_channel_predictors() {
        let mut block = [0u8; 72];
        // Left predictor = 100, step_index = 0.
        block[0] = 100;
        block[1] = 0;
        // Right predictor = -200, step_index = 0.
        block[4] = (-200i16 as u16 & 0xff) as u8;
        block[5] = ((-200i16 as u16) >> 8) as u8;
        let pcm = decode_stereo(&block);
        // Interleaved L, R: pcm[0] = L's first sample, pcm[1] = R's.
        assert_eq!(pcm[0], 100);
        assert_eq!(pcm[1], -200);
    }

    mod units {
        use crate::audio::units::*;

        #[test]
        fn rate_ratio_unit_is_identity() {
            let r = SampleRate::new(36000);
            assert_eq!(RateRatio::UNIT.apply(r), r);
        }

        /// 0xAADA observed in 0_0_3 LM2 entries; 16:16 fixed-point ~ 0.6674x.
        /// Engine math is `(rate * ratio) / 0x10000` in u64 with truncation.
        /// Pin the exact division so a future move to floating-point or
        /// different rounding surfaces here.
        #[test]
        fn rate_ratio_observed_lm2_scaling() {
            let nominal = SampleRate::new(16000);
            let scaled = RateRatio::new(0xAADA).apply(nominal);
            assert_eq!(scaled.hz(), (16000u64 * 0xAADA / 0x10000) as u32);
            assert_eq!(scaled.hz(), 10678);
        }

        /// Above-unity ratios push past the input rate; the u64 widening
        /// guards against the 32-bit overflow you'd hit if you multiplied
        /// in u32 (44100 * 0x20000 = 0x564f8000, which fits, but the
        /// intermediate is asking for trouble for higher rates).
        #[test]
        fn rate_ratio_above_unity_widens() {
            // ratio = 2.0
            let scaled = RateRatio::new(0x20000).apply(SampleRate::new(44100));
            assert_eq!(scaled.hz(), 88200);
        }

        #[test]
        fn rate_ratio_zero_is_zero() {
            assert!(RateRatio::new(0).is_zero());
            assert!(!RateRatio::UNIT.is_zero());
        }

        #[test]
        fn nibble_wraps_to_four_bits() {
            assert_eq!(Nibble::new(0xFF).value(), 0x0F);
            assert_eq!(Nibble::new(0x10).value(), 0x00);
            assert_eq!(Nibble::new(0x07).value(), 0x07);
        }

        #[test]
        fn nibble_lo_hi_split_a_byte() {
            assert_eq!(Nibble::lo(0xA3).value(), 0x03);
            assert_eq!(Nibble::hi(0xA3).value(), 0x0A);
        }

        #[test]
        fn sample_rate_display_includes_hz() {
            assert_eq!(SampleRate::new(36000).to_string(), "36000 Hz");
        }

        #[test]
        fn seq_id_lower_hex_strips_type_tag_via_caller_mask() {
            // SeqId itself doesn't mask; the SM2 parser does `& 0x00FFFFFF`
            // before indexing array_b. Pin the raw round-trip so a future
            // change to mask inside SeqId surfaces here.
            let s = SeqId::new(0x4000_007E);
            assert_eq!(s.raw(), 0x4000_007E);
            assert_eq!(format!("{s:x}"), "4000007e");
        }
    }

    mod enums {
        use crate::audio::codec3;
        use crate::audio::codec8;

        #[test]
        fn codec3_mode_round_trip() {
            for v in 0u8..=255 {
                match codec3::Mode::from_u8(v) {
                    Some(m) => assert_eq!(m.as_u8(), v),
                    None => assert!(v >= 2, "unexpected None at {v}"),
                }
            }
        }

        #[test]
        fn codec3_mode_channel_counts() {
            assert_eq!(codec3::Mode::PlanarMono.channels(), 1);
            assert_eq!(codec3::Mode::InterleavedStereo.channels(), 2);
        }

        /// Engine accepts 1, 2, 4, 6 channels and rejects everything else
        /// in `sub_19b4a0` with the `"Adpcm allows only sound files with
        /// 1, 2, 4 and 6 channels"` string. Pin the exact set.
        #[test]
        fn codec8_channels_match_engine_allow_list() {
            for v in [1, 2, 4, 6] {
                let c = codec8::Channels::from_u32(v).expect("accepted");
                assert_eq!(c.count() as u32, v);
            }
            for v in [0, 3, 5, 7, 8, 16, 0xFFFFFFFF] {
                assert!(
                    codec8::Channels::from_u32(v).is_err(),
                    "channels={v} should be rejected"
                );
            }
        }

        #[test]
        fn codec8_kernel_kind_branch() {
            assert_eq!(codec8::KernelKind::from_u32(1), codec8::KernelKind::FourBit);
            for v in [0, 2, 3, 99, 0xFFFF_FFFF] {
                assert_eq!(codec8::KernelKind::from_u32(v), codec8::KernelKind::SixBit);
            }
        }
    }
}
