//! Xbox-ADPCM (XbA) decoder, format `wFormatTag = 0x0069`.
//!
//! Identified from `splintercell.xbe` static analysis: `sub_1885a0`
//! sets up a `WAVEFORMATEX` with `wFormatTag = 0x69`,
//! `wBitsPerSample = 4`, `cbSize = 2`, and `nBlockAlign = nChannels *
//! 36`, then calls `IDirectSound::CreateSoundBuffer` and hands the
//! audio bytes to the MCPX hardware decoder via `sub_18cb60`
//! (`IDirectSoundBuffer::SetBufferData`). A QEMU plugin trace
//! capture confirmed the `SetBufferData` payload is identical to the
//! corresponding bytes in the source `.SM2` file — no software
//! re-encoding step.
//!
//! The earlier "DARE-IMA-ADPCM v3" port in this file targeted the
//! PC-build's CPU decoder (`sub_1942e0` / `sub_1946d0`) — code that
//! is present in `splintercell.xbe` but not executed at runtime on
//! Xbox. That code is gone; the math here uses the standard MS
//! IMA-ADPCM step-size table, which is what XbA shares with all
//! other IMA-ADPCM variants.
//!
//! Block layout per channel:
//!   - 2 bytes: predictor (i16 LE) — initial sample for the block
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
/// IMA-ADPCM variant including XbA — the table has been frozen since
/// the original IMA spec.
const STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17,
    19, 21, 23, 25, 28, 31, 34, 37, 41, 45,
    50, 55, 60, 66, 73, 80, 88, 97, 107, 118,
    130, 143, 157, 173, 190, 209, 230, 253, 279, 307,
    337, 371, 408, 449, 494, 544, 598, 658, 724, 796,
    876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066,
    2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358,
    5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899,
    15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];

/// Standard MS IMA-ADPCM step-index adjustment table. Indexed by the
/// 4-bit nibble. Note that the magnitude bits (0..3) of the nibble
/// share the same adjustment for the sign-positive (0..7) and
/// sign-negative (8..15) halves.
const INDEX_TABLE: [i8; 16] = [
    -1, -1, -1, -1, 2, 4, 6, 8,
    -1, -1, -1, -1, 2, 4, 6, 8,
];

#[derive(Copy, Clone, Debug)]
struct ChannelState {
    predictor: i32,
    step_index: i32,
}

impl ChannelState {
    fn from_header(header: [u8; 4]) -> Self {
        let predictor = i16::from_le_bytes([header[0], header[1]]) as i32;
        let step_index = (header[2] as i32).clamp(0, 88);
        Self { predictor, step_index }
    }

    /// Decode one 4-bit nibble into a sample, advancing predictor and
    /// step_index. Uses the canonical multiply-then-shift form
    /// `diff = ((2*magnitude + 1) * step) >> 3`. Bit-by-bit shifted
    /// accumulation (`(step>>3) + (step>>2)*bit0 + (step>>1)*bit1 +
    /// step*bit2`) is mathematically equivalent in real arithmetic
    /// but loses precision per term under integer truncation when
    /// `step` is small — for `step=7`, bit-form gives `1` and the
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
        self.step_index = (self.step_index
            + INDEX_TABLE[nibble as usize] as i32)
            .clamp(0, 88);
        self.predictor as i16
    }
}

/// Decode a single mono XbA block. `block` must be exactly 36 bytes.
/// Output is 64 samples — the predictor is the seed for decoding the
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
        data.len() % BLOCK_BYTES_PER_CHANNEL == 0,
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
        data.len() % STEREO_BLOCK == 0,
        "stereo XbA stream length {} is not a multiple of {}",
        data.len(),
        STEREO_BLOCK,
    );
    let block_count = data.len() / STEREO_BLOCK;
    let mut out = Vec::with_capacity(block_count * SAMPLES_PER_BLOCK * 2);
    for chunk in data.chunks_exact(STEREO_BLOCK) {
        let mut left = ChannelState::from_header([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let mut right = ChannelState::from_header([chunk[4], chunk[5], chunk[6], chunk[7]]);
        // 8 rounds × 4 data bytes per channel × 2 nibbles per byte = 64 samples per channel.
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

/// Codec id 3 — the streamed-audio variant used by `.SS2`/`.LS2`
/// files. Distinct from the SM2/LM2 hardware-decoded XbA above:
/// codec 3 streams are CPU-decoded by the engine, going through the
/// `sub_193950` Decode entry which calls `sub_193780` (InitHeader)
/// once and `sub_193510` per refill. The per-block kernel is
/// `sub_198600`, a thin dispatcher that routes by the mode flag in
/// the header (file byte 12, latched into codec state at `+0x54`):
///
///   - `mode == 0`  → `sub_1983a0`  one-channel-at-a-time decode
///                   (used for mono streams and planar stereo).
///   - `mode == 1`  → `sub_1984a0`  L/R nibbles interleaved within
///                   each byte (the byte's low nibble is L, high
///                   nibble is R), output written interleaved.
///
/// Both kernels run on STANDARD IMA-ADPCM math:
///   `delta = ((nibble & 7) * 2 + 1) * step >> 3`
///   `predictor += (nibble & 8) ? -delta : +delta`
///   `predictor.clamp(-32768, 32767)`
///   `step_index += INDEX_TABLE[nibble & 0xf]`
///   `step_index.clamp(0, 88)`
///
/// confirmed by reading the `data_2d05c0` (16-entry index table) and
/// `data_2d0600` (89-entry step table) data sections — they're
/// byte-for-byte identical to `INDEX_TABLE` and `STEP_TABLE` above.
/// The codec ships the tables inline rather than reusing a single
/// copy, but the values match.
///
/// We expose only the kernels here; the per-stream state machine
/// (`sub_193950`'s pad-sample handling for output-count parity) is
/// for incremental Decode-call boundaries and isn't needed when
/// decoding a stream end-to-end.
pub mod codec3 {
    use super::{INDEX_TABLE, STEP_TABLE};

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
    /// `sub_1984a0` exactly — same pre-step lookup, same clamp
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
        state.step_index = (state.step_index + INDEX_TABLE[(nibble & 0xf) as usize] as i32)
            .clamp(0, 88);
        state.predictor as i16
    }

    /// Decode `count` samples for one channel from `data`. Each input
    /// byte yields two samples — DARE's IMA-ADPCM variant emits the
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
    /// and one R sample (LOW nibble) — DARE's `sub_1984a0`
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
    /// some fields as big-endian u16 — we interpret them here as
    /// they reach the decoder, not as raw file bytes.
    ///
    /// Layout (file offsets):
    ///   `+0x00`  u8   version          — must be 3.
    ///   `+0x01..+0x03`  3 bytes        — sample-count high bits.
    ///   `+0x04..+0x07`  f32 LE         — frame-period or scaling
    ///                                    factor (≈ 0.06 in observed
    ///                                    files).
    ///   `+0x08..+0x0B`  u32 LE         — `track_count`: number of
    ///                                    SEQUENTIAL ADPCM clips
    ///                                    packed into the file.
    ///                                    The engine plays them in
    ///                                    order as the mission
    ///                                    progresses — they are not
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
    ///                                    step_index=0) — they are
    ///                                    independent, just played
    ///                                    back-to-back.
    ///   `+0x0C`  u8                    — mode flag latched into
    ///                                    `+0x54`. 0 = planar /
    ///                                    `sub_1983a0` per stream,
    ///                                    1 = interleaved-stereo /
    ///                                    `sub_1984a0` (true stereo,
    ///                                    used for music tracks).
    ///   `+0x0E..+0x0F`  big-endian u16 — `+0x40` field, used by
    ///                                    `sub_193510` as a
    ///                                    block-availability counter
    ///                                    (decremented on each
    ///                                    inner-buffer fill).
    #[derive(Clone, Debug)]
    pub struct Header {
        pub version: u8,
        /// Count of sequential ADPCM clips concatenated in the
        /// file (NOT stereo channels). See struct doc.
        pub track_count: u32,
        pub mode: u8,
        /// f32 at file offset `+0x04`. Empirically maps to sample
        /// rate via the `header_sample_rate()` helper.
        pub block_period: f32,
        /// Per-channel initial ADPCM state. The engine's
        /// `sub_193780` byte-swaps the u16 fields at file
        /// offsets `+0x10` (L predictor) and `+0x14` (R predictor)
        /// from BE → LE on load, then `sub_193510` copies them
        /// into the live state at every block-refill. The kernel
        /// expects each block to begin with these seeds, not
        /// `(0, 0)`. Field layout in the file:
        ///   `+0x10..+0x11` BE u16 → L predictor (signed)
        ///   `+0x12` u8           → L step_index
        ///   `+0x14..+0x15` BE u16 → R predictor (signed)
        ///   `+0x16` u8           → R step_index
        pub init_l: ChannelState,
        pub init_r: ChannelState,
    }

    /// Bytes the engine reads as the codec_id=3 header (matches
    /// `sub_193780`'s `var_10c = 0x1c`).
    pub const HEADER_BYTES: usize = 28;

    /// File offset where ADPCM data begins. The engine reads
    /// exactly 28 bytes (`HEADER_BYTES`) for the header in
    /// `sub_193780`, then `sub_193510` reads the rest of the
    /// file sequentially and feeds it to `sub_198600` — i.e.
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
        let track_count = u32::from_le_bytes([
            file_bytes[8],
            file_bytes[9],
            file_bytes[10],
            file_bytes[11],
        ]);
        // track_count = 0 is observed on most files (dialogue
        // and the Music_*.SS2 stereo set). The handful with
        // non-zero values (`0_0=1, 0_0_2=2, 1_1_0=3,
        // Music_Common.LS2=3, 4_2=5_1=7`) all use it as
        // game-script metadata — not byte structure.
        // Sanity bound; observed range is 0..=17 across retail
        // standalone files and STREAM.SS2 sub-streams. 256 is a
        // soft upper limit to reject obvious garbage headers.
        if track_count > 256 {
            return Err("track_count out of range");
        }
        let mode = file_bytes[12];
        if mode > 1 {
            return Err("unsupported mode (must be 0 or 1)");
        }
        let block_period = f32::from_le_bytes([
            file_bytes[4],
            file_bytes[5],
            file_bytes[6],
            file_bytes[7],
        ]);
        // Initial ADPCM state. A QEMU plugin trace hooking
        // `sub_198600` (the per-block kernel) showed the engine
        // enters every block with state seeded to (0, 0, 0, 0)
        // — NOT the BE u16 of file +0x10..+0x17 that an earlier
        // version of this code extracted. The bytes at
        // +0x10..+0x17 ARE non-zero in some files (e.g.
        // Music_Common.SS2 has `ff cb 1d 00 ff ce 1a 02`) but
        // they're per-stream metadata, not predictor seeds.
        // State evolution across blocks is continuous; the 21%
        // of trace captures with state == (0, 0, 0, 0) line up
        // with natural quiet passages where step_index decays to
        // 0 and predictor returns near zero, not periodic resets.
        let init_l = ChannelState { predictor: 0, step_index: 0 };
        let init_r = ChannelState { predictor: 0, step_index: 0 };
        Ok(Header {
            version: 3,
            track_count,
            mode,
            block_period,
            init_l,
            init_r,
        })
    }

    /// Source sample rate for codec_id=3 streams. Hardcoded to
    /// 36000 Hz in the engine: `sub_182b50` (audio-system init
    /// called from `sub_1806a0`) builds a `WAVEFORMATEX` with
    /// `wFormatTag=1` (PCM), `nChannels=2`, `wBitsPerSample=16`,
    /// `nSamplesPerSec=0x8CA0` (= 36000). No per-file rate is
    /// encoded in the codec_id=3 header:
    ///   - file `+0x04..+0x07` (the `block_period` f32) varies
    ///     across files but is consumed elsewhere — not as a rate
    ///   - file `+0x0E..+0x0F` is constant `00 0a` across every
    ///     retail file inspected; `sub_193780` byte-swaps it to
    ///     `0x0A00 = 2560` and stores at codec+0x40, where
    ///     `sub_193510` decrements it on each refill (a remaining-
    ///     bytes counter, not a rate)
    ///   - the QEMU plugin trace of `sub_230dc0` captured
    ///     44100/48000 — the DSound mixer's *output* rate, not
    ///     the codec's source rate
    /// The `block_period` f32 is retained on the parsed header
    /// for forensic purposes but plays no role in playback rate.
    /// User-validated 36000 by ear on `0_0_2.LS2` and
    /// `Music_Common.SS2`; `Music_Birmanie.SS2` perceptual issues
    /// at the end of the file are caused by the open mode=1
    /// late-stream blow-out (saturating predictor drift), not
    /// rate mismatch.
    pub fn header_sample_rate(_header: &Header) -> u32 {
        36000
    }

    /// Decode a complete codec_id=3 file. Returns mono i16 PCM
    /// for `mode=0` (the dominant case — dialogue and most LS2
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
    /// ADPCM state to (0,0) at clip boundaries — either by
    /// design or because dialogue clips end in silence — so a
    /// single continuous decode is equivalent to per-track
    /// resets. `track_count` is therefore metadata (script cue
    /// counts, streaming-subsystem chunking) rather than a
    /// byte-layout signal.
    /// Decode with periodic ADPCM-state reset to `(0, 0, 0, 0)`
    /// every `reset_blocks` blocks. Block size = 1440 frames
    /// (mode=1 stereo: 1440 bytes; mode=0 mono: 720 bytes,
    /// 1440 samples).
    ///
    /// This is exploratory — the actual engine reset cadence
    /// isn't yet known. Use to A/B different cadences against
    /// late-stream drift.
    pub fn decode_file_with_reset(
        file_bytes: &[u8],
        reset_blocks: usize,
    ) -> Result<Vec<i16>, &'static str> {
        let header = parse_header(file_bytes)?;
        if file_bytes.len() < DATA_OFFSET {
            return Err("file too short for ADPCM payload");
        }
        if reset_blocks == 0 {
            return Err("reset_blocks must be >= 1");
        }
        let data = &file_bytes[DATA_OFFSET..];
        match header.mode {
            0 => {
                // mode=0 mono: 720 bytes per block. Reset every
                // `reset_blocks` blocks → byte_chunk = 720 * N.
                let bytes_per_chunk = 720 * reset_blocks;
                let mut out = Vec::with_capacity(data.len() * 2);
                let mut offset = 0;
                while offset < data.len() {
                    let end = (offset + bytes_per_chunk).min(data.len());
                    let chunk = &data[offset..end];
                    let mut state = ChannelState::new(0, 0);
                    let samples = decode_planar(chunk, &mut state, chunk.len() * 2);
                    out.extend_from_slice(&samples);
                    offset = end;
                }
                Ok(out)
            }
            1 => {
                // mode=1 stereo: 1440 bytes per block.
                let bytes_per_chunk = 1440 * reset_blocks;
                let mut out = Vec::with_capacity(data.len() * 2);
                let mut offset = 0;
                while offset < data.len() {
                    let end = (offset + bytes_per_chunk).min(data.len());
                    let chunk = &data[offset..end];
                    let mut state = [ChannelState::new(0, 0), ChannelState::new(0, 0)];
                    let samples = decode_interleaved_stereo(chunk, &mut state, chunk.len());
                    out.extend_from_slice(&samples);
                    offset = end;
                }
                Ok(out)
            }
            _ => Err("unsupported mode (must be 0 or 1)"),
        }
    }

    pub fn decode_file(file_bytes: &[u8]) -> Result<Vec<i16>, &'static str> {
        let header = parse_header(file_bytes)?;
        if file_bytes.len() < DATA_OFFSET {
            return Err("file too short for ADPCM payload");
        }
        let data = &file_bytes[DATA_OFFSET..];

        match header.mode {
            0 => {
                // mode=0 mono: the engine's sub_193780 still
                // populates the L seed (file +0x10..+0x12) so we
                // honor it here. R seed unused.
                let mut state = header.init_l;
                Ok(decode_planar(data, &mut state, data.len() * 2))
            }
            1 => {
                // mode=1 stereo: both per-channel seeds drive
                // the kernel. Without them the predictor walks
                // from a wrong starting point and accumulates
                // saturation drift over long streams (audible as
                // "blown-out" distortion late in the track on
                // Music_Birmanie.SS2 etc.).
                let mut state = [header.init_l, header.init_r];
                Ok(decode_interleaved_stereo(data, &mut state, data.len()))
            }
            _ => Err("unsupported mode (must be 0 or 1)"),
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
            // L: nibble=0, step=7, delta=0 → predictor stays 100.
            assert_eq!(out[0], 100);
            // R: nibble=7, step=7, delta=((15)*7)>>3 = 13, predictor -200+13 = -187.
            assert_eq!(out[1], -187);
        }

        /// Stereo state-independence: when both channels receive the
        /// same nibble sequence (byte 0x77 → L=7, R=7) their states
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
        /// nibbles — the symmetric test above isn't masking a
        /// state-leak bug.
        #[test]
        fn stereo_channels_diverge_on_asymmetric_input() {
            let mut state = [ChannelState::new(0, 0), ChannelState::new(0, 0)];
            let _ = decode_interleaved_stereo(&[0x70, 0x70], &mut state, 2);
            // L saw nibble 7 twice → predictor=43, step_index=16.
            assert_eq!(state[0].predictor, 43);
            // R saw nibble 0 twice → predictor stays 0, step_index
            // tries to go negative each step but clamps to 0.
            assert_eq!(state[1].predictor, 0);
            assert_eq!(state[1].step_index, 0);
        }
    }
}

/// Multi-bank `.SS2` containers (e.g. `STREAM.SS2`) — a flat sequence
/// of self-describing banks, each prefixed with a 0x2C-byte wrapper
/// followed by **three** codec_id=3 sub-streams in fixed-layout order:
///
/// ```text
///   +0x000..+0x02B  bank wrapper (44 bytes)
///   +0x02C..+0x195  sub-stream 0  (preview clip,  362 bytes = 0x16A)
///   +0x196..+0x2FE  sub-stream 1  (preview clip,  361 bytes = 0x169)
///   +0x2FF..+(end)  sub-stream 2  (main music,    bank_size - 0x2FF)
/// ```
///
/// Each sub-stream is a complete standalone-style codec_id=3 stream
/// (28-byte header + ADPCM nibbles from +0x1C onwards), suitable
/// for `codec3::parse_header` / `codec3::decode_file`.
///
/// Sub 0/1 are 9 ms previews (likely cue / loop fingerprints used
/// by the engine for state matching); sub 2 is the actual music
/// stream that should be played for human listening.
///
/// Wrapper layout (verified across all 97 banks of the retail
/// `STREAM.SS2`):
///
/// ```text
///   +0x00  u32  magic_a       = 2          (constant)
///   +0x04  u32  codec_id      = 3          (constant)
///   +0x08  u32  bank_size                  ← total bytes from this
///                                              wrapper to the next
///                                              (or EOF). Only
///                                              varying field.
///   +0x0C  u32                = 0x14       (constant)
///   +0x10  u32                = 0x450      (constant)
///   +0x14  u32                = 0x169      (constant; matches sub 1 size)
///   +0x18  u32                = 1          (constant)
///   +0x1C  u32                = 0x14       (constant)
///   +0x20  u32                = 0x16A      (constant; matches sub 0 size)
///   +0x24  u32                = 0x169      (constant; matches sub 1 size)
///   +0x28  u32                = 0x169      (constant)
/// ```
///
/// Single-stream `.SS2` / `.LS2` files (e.g. `Music_Common.SS2`,
/// `0_0_2.LS2`) start with the codec_id=3 byte directly at file
/// offset 0 and don't carry the wrapper. Use this module only for
/// multi-bank containers.
pub mod ss2 {
    use super::codec3;

    /// Size of the per-bank wrapper that precedes the sub-streams.
    pub const WRAPPER_BYTES: usize = 0x2C;

    /// `+0x08` field of the wrapper; total byte size of this bank
    /// including wrapper.
    pub const BANK_SIZE_OFFSET: usize = 0x08;

    /// Sub-stream offsets within a bank (constant across all 97 banks
    /// of retail `STREAM.SS2`).
    pub const SUB0_OFFSET: usize = 0x02C;
    pub const SUB1_OFFSET: usize = 0x196;
    pub const SUB2_OFFSET: usize = 0x2FF;

    /// One parsed bank. `main_payload` is sub-stream 2 (the actual
    /// music). `previews` holds sub-streams 0 and 1 (9 ms preview
    /// clips, likely engine-internal cue fingerprints).
    #[derive(Clone, Debug)]
    pub struct Bank<'a> {
        pub index: usize,
        /// Absolute file offset of the wrapper.
        pub offset: usize,
        /// Total bank length (wrapper + sub-streams), from the
        /// wrapper's `+0x08` field.
        pub bank_size: usize,
        /// Sub-stream 2: the main music stream. Begins with the
        /// codec_id=3 byte and is the one playback should target.
        pub main_payload: &'a [u8],
        /// Sub-streams 0 and 1 (each ~360 bytes / 9 ms). Provided
        /// for forensic completeness; not normally played.
        pub previews: [&'a [u8]; 2],
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
            let magic_a = u32::from_le_bytes(
                file_bytes[cursor..cursor + 4].try_into().unwrap(),
            );
            let codec_id = u32::from_le_bytes(
                file_bytes[cursor + 4..cursor + 8].try_into().unwrap(),
            );
            if magic_a != 2 || codec_id != 3 {
                return Err("bank wrapper signature mismatch");
            }
            let bank_size = u32::from_le_bytes(
                file_bytes[cursor + BANK_SIZE_OFFSET
                    ..cursor + BANK_SIZE_OFFSET + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if bank_size <= SUB2_OFFSET || cursor + bank_size > file_bytes.len() {
                return Err("bank size walks past EOF or is too small for the sub-stream layout");
            }
            let bank_bytes = &file_bytes[cursor..cursor + bank_size];
            let sub0 = &bank_bytes[SUB0_OFFSET..SUB1_OFFSET];
            let sub1 = &bank_bytes[SUB1_OFFSET..SUB2_OFFSET];
            let sub2 = &bank_bytes[SUB2_OFFSET..];
            out.push(Bank {
                index: out.len(),
                offset: cursor,
                bank_size,
                main_payload: sub2,
                previews: [sub0, sub1],
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

    /// Decode the main sub-stream of a bank. Convenience wrapper
    /// that hands `bank.main_payload` to `codec3::decode_file`.
    pub fn decode_bank(bank: &Bank<'_>) -> Result<Vec<i16>, &'static str> {
        codec3::decode_file(bank.main_payload)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn rejects_signature_mismatch() {
            let mut bytes = vec![0u8; SUB2_OFFSET + 4];
            bytes[0] = 1; // magic_a wrong
            assert!(list(&bytes).is_err());
        }

        #[test]
        fn rejects_bank_size_past_eof() {
            let mut bytes = vec![0u8; SUB2_OFFSET + 4];
            bytes[0..4].copy_from_slice(&2u32.to_le_bytes());
            bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&0xFFFFFFu32.to_le_bytes());
            assert!(list(&bytes).is_err());
        }

        #[test]
        fn parses_synthetic_bank_with_substream_layout() {
            // One bank: wrapper + sub0 (362 B) + sub1 (361 B) +
            // sub2 (4 B of dummy payload starting with codec_id=3).
            let bank_size: u32 = (SUB2_OFFSET + 4) as u32;
            let mut bytes = vec![0u8; bank_size as usize];
            bytes[0..4].copy_from_slice(&2u32.to_le_bytes());
            bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&bank_size.to_le_bytes());
            bytes[SUB0_OFFSET] = 0x03;
            bytes[SUB1_OFFSET] = 0x03;
            bytes[SUB2_OFFSET..SUB2_OFFSET + 4]
                .copy_from_slice(&[0x03, 0xaa, 0xbb, 0xcc]);
            let banks = list(&bytes).expect("parse");
            assert_eq!(banks.len(), 1);
            let b = &banks[0];
            assert_eq!(b.bank_size, bank_size as usize);
            assert_eq!(b.main_payload, &[0x03, 0xaa, 0xbb, 0xcc]);
            assert_eq!(b.previews[0].len(), SUB1_OFFSET - SUB0_OFFSET);
            assert_eq!(b.previews[1].len(), SUB2_OFFSET - SUB1_OFFSET);
            assert_eq!(b.previews[0][0], 0x03);
            assert_eq!(b.previews[1][0], 0x03);
        }
    }
}

/// `.SM2` (Maps BigFile) outer directory parsing. Format is reversed
/// from `sub_17e250` (loads/validates the directory) and `sub_17e3f0`
/// (linear search by map name). The format:
///
///   offset 0x00  u32  version              — must be 7 in SC1 NTSC
///   offset 0x04  u32  record_table_offset  — typically 0x0c
///   offset 0x08  u32  record_count         — number of records
///   offset record_table_offset:
///     repeat record_count times {
///       u32  reserved_0  (always 0 on disk; runtime field)
///       u32  reserved_1  (always 0 on disk; runtime field)
///       u32  data_offset — absolute file offset of this map's data
///       u32  data_size   — bytes to read at `data_offset`
///       char name[0x20]  — null-terminated map name (e.g. "0_0_2_Training")
///     }  // 0x30 bytes per record
///
/// `sub_17e470` allocates `data_size` bytes, seeks to `data_offset`,
/// reads `data_size` bytes — that's the per-map descriptor blob,
/// itself a fixup-relative pointer-graph. See AUDIO.md for the next
/// layer.
pub mod sm2 {
    use std::io::{self, Read, Seek, SeekFrom};

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
    /// difference between consecutive entries' offsets — the last
    /// entry's length is the gap from its offset to the start of the
    /// next map's data section.
    ///
    /// `sample_rate` and `channels` are pulled from the metadata
    /// record at `array_b[seq_id]` (120-byte struct containing the
    /// source `.wav` name, sample count, byte length, sample rate,
    /// and average bytes-per-second).
    #[derive(Clone, Debug)]
    pub struct SoundEntry {
        pub seq_id: u32,
        pub file_offset: u64,
        pub length: u64,
        pub sample_rate: u32,
        pub channels: u16,
        pub source_name: String,
    }

    const ARRAY_B_ENTRY_SIZE: usize = 120;
    const ARRAY_B_OFFSET_SAMPLE_RATE: usize = 0x44;
    const ARRAY_B_OFFSET_AVG_BYTES_PER_SEC: usize = 0x40;
    /// Channel count lives in the HIGH word of the u32 at `+0x48`
    /// (i.e. read u16 at `+0x4a`). The low word is `0x0010` for all
    /// observed entries — likely a fixed format-flags constant.
    /// Verified: FN20IS_1 has `0x00020010` here (stereo, 44.1kHz —
    /// matches its 72-byte block alignment) while every mono entry
    /// has `0x00010010`.
    const ARRAY_B_OFFSET_CHANNELS: usize = 0x4a;
    const ARRAY_B_OFFSET_NAME: usize = 0x50;
    const ARRAY_B_NAME_BYTES: usize = 16;

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
    pub fn parse_sound_table(
        descriptor: &[u8],
        record: &Record,
        next_record_offset: Option<u32>,
    ) -> io::Result<Vec<SoundEntry>> {
        if descriptor.len() < 0x24 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "descriptor too small for top header",
            ));
        }
        let array_b_off =
            u32::from_le_bytes(descriptor[0x0c..0x10].try_into().unwrap()) as usize;
        let array_b_cnt =
            u32::from_le_bytes(descriptor[0x10..0x14].try_into().unwrap());
        let array_c_off =
            u32::from_le_bytes(descriptor[0x14..0x18].try_into().unwrap()) as usize;
        let array_c_cnt =
            u32::from_le_bytes(descriptor[0x18..0x1c].try_into().unwrap());
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
        let audio_end = next_record_offset
            .map(|n| n as u64)
            .unwrap_or(u64::MAX);

        let mut entries: Vec<(u32, u32)> = Vec::with_capacity(sub_a_cnt);
        for i in 0..sub_a_cnt {
            let off = sub_a_off + i * 8;
            let seq_id =
                u32::from_le_bytes(descriptor[off..off + 4].try_into().unwrap());
            let rel =
                u32::from_le_bytes(descriptor[off + 4..off + 8].try_into().unwrap());
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

            // Look up per-sound metadata in array_b[seq_id]. Each entry
            // is 120 bytes; +0x44 = sample_rate, +0x40 = avg
            // bytes-per-sec, +0x4a = channel count (u16 high word of
            // the u32 at +0x48), +0x50 = 16-byte source `.wav` name.
            let (sample_rate, channels, source_name) = if (seq_id as usize) < array_b_cnt as usize {
                let entry_off = array_b_off + (seq_id as usize) * ARRAY_B_ENTRY_SIZE;
                if entry_off + ARRAY_B_ENTRY_SIZE <= descriptor.len() {
                    let entry = &descriptor[entry_off..entry_off + ARRAY_B_ENTRY_SIZE];
                    let sr = u32::from_le_bytes(
                        entry[ARRAY_B_OFFSET_SAMPLE_RATE..ARRAY_B_OFFSET_SAMPLE_RATE + 4]
                            .try_into()
                            .unwrap(),
                    );
                    let ch = u16::from_le_bytes(
                        entry[ARRAY_B_OFFSET_CHANNELS..ARRAY_B_OFFSET_CHANNELS + 2]
                            .try_into()
                            .unwrap(),
                    );
                    // Clamp to supported channel counts; fall back to mono
                    // for anything weird.
                    let ch = if ch == 1 || ch == 2 { ch } else { 1 };
                    let name_bytes = &entry[ARRAY_B_OFFSET_NAME..ARRAY_B_OFFSET_NAME + ARRAY_B_NAME_BYTES];
                    let name_end = name_bytes
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(ARRAY_B_NAME_BYTES);
                    let name = String::from_utf8_lossy(&name_bytes[..name_end]).into_owned();
                    (sr, ch, name)
                } else {
                    (22050, 1u16, String::new())
                }
            } else {
                (22050, 1u16, String::new())
            };

            out.push(SoundEntry {
                seq_id,
                file_offset,
                length,
                sample_rate,
                channels,
                source_name,
            });
        }
        Ok(out)
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
            let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(NAME_BYTES);
            let name = String::from_utf8_lossy(&name_bytes[..name_end]).into_owned();
            records.push(Record {
                data_offset,
                data_size,
                name,
            });
        }
        Ok(records)
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

    /// Symmetric to the above — all 0x0f nibbles (max negative) must
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
}
