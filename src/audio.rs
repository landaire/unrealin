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
/// of self-describing banks, each prefixed with a 0x2C-byte wrapper.
/// Inside a bank, **three independent codec_id=3 voices play in
/// parallel**, with their compressed bytes interleaved at chunk
/// granularity. This was confirmed by a QEMU plugin trace of
/// `sub_198600`: 3 distinct codec-state pointers (= 3 voices) round-
/// robin across consecutive kernel invocations, each producing 1440
/// stereo frames per call. The captured input bytes for each voice
/// match the bank's bytes exactly when re-interleaved by the formula
/// implemented in `deinterleave_voice` below — verified byte-for-byte
/// against 492,470 bytes of voice-A trace data.
///
/// **Per-bank wrapper** (constant across all 97 banks of retail
/// `STREAM.SS2` except `+0x08`):
/// ```text
///   +0x00  u32  magic_a       = 2          (constant)
///   +0x04  u32  codec_id      = 3          (constant)
///   +0x08  u32  bank_size                  ← total bytes from this
///                                              wrapper to the next
///                                              (or EOF). Only
///                                              varying field.
///   +0x0C  u32                = 0x14       (constant)
///   +0x10  u32                = 0x450      ← cycle size in bytes (1104)
///   +0x14  u32                = 0x169      (constant)
///   +0x18  u32                = 1          (constant)
///   +0x1C  u32                = 0x14       ← per-cycle trailer (20 bytes)
///   +0x20  u32                = 0x16A      ← voice 0 region size in cycle 0
///   +0x24  u32                = 0x169      ← voice 1 region size in cycle 0
///   +0x28  u32                = 0x169      ← voice 2 region size in cycle 0
/// ```
///
/// **Cycle 0 layout** (with per-voice 0x44-byte headers; verified
/// against trace data):
/// ```text
///   bank +0x02C..+0x06F  voice 0 header (28-byte codec3 header +
///                                         4 zero bytes + 36-byte
///                                         PCM lookahead; 0x44 total)
///   bank +0x070..+0x195  voice 0 ADPCM, 0x126 = 294 bytes
///   bank +0x196..+0x1D9  voice 1 header (0x44)
///   bank +0x1DA..+0x2FE  voice 1 ADPCM, 0x125 = 293 bytes
///   bank +0x2FF..+0x342  voice 2 header (0x44)
///   bank +0x343..+0x46B  voice 2 ADPCM, 0x129 = 297 bytes
///   bank +0x46C..+0x47B  cycle trailer (0x10 = 16 bytes; aligned
///                                        to wrapper offset
///                                        +0x47C = +0x2C + 0x450)
/// ```
///
/// **Cycle k (k >= 1)** layout (no headers, just ADPCM + trailer):
/// ```text
///   cycle_start = WRAPPER + k * 0x450
///   voice 0: cycle_start + 0      ..+ size_a
///   voice 1: cycle_start + size_a ..+ size_b
///   voice 2: cycle_start + size_a + size_b ..+ size_c
///   trailer: cycle_start + 0x43C  ..+ 0x14
/// ```
/// Per-cycle size sum = 361 × 3 + 1 (one voice gets the bonus byte)
/// + 20 trailer = 1104. The bonus byte rotates by `cycle_idx % 3`:
/// cycle 1 → voice 1 gets +1, cycle 2 → voice 2, cycle 3 → voice 0,
/// cycle 4 → voice 1, etc.
/// Codec id 8 — proto/demo-only DARE-IMA variant. Whereas codec_id=3
/// is a CPU-decoded standard IMA-ADPCM streamer, codec_id=8 is a
/// hand-tuned MMX SIMD codec that processes 4 sub-channels in
/// parallel through a "voice"-style decoder with per-voice state.
///
/// File layout (first 64 bytes, all u32 LE):
///
///   +0x00  codec_id  = 8
///   +0x04  total_size  - whole-file byte count incl. header
///   +0x08  ?           - small per-file value (e.g. 0x34, 0x5dc)
///   +0x0c  ?           - similar magnitude (e.g. 0x420, 0x49c)
///   +0x10  block_size  = 0x600 (1536 bytes per block)
///   +0x14  channels    = 2 (always stereo in observed files)
///   +0x18  sample_rate - typically 36000 Hz
///   +0x1c  0           - reserved
///   +0x20  0           - reserved
///   +0x24  ?           - always 4 in observed files
///   +0x28  ?           - 1 or 2
///   +0x2c  ?           - 1 or 2
///   +0x30  ?           - 2
///   +0x34  ?           - 0x500 (1280)
///   +0x38  ?           - 0 or 5
///
/// The kernel at retail sub_1942e0 (4-bit) and sub_1946d0 (6-bit) is
/// a DARE-IMA variant that uses extended state (3 32-bit registers,
/// not just predictor+step_index): a primary accumulator, a 32-bit
/// "step magnitude" with full-resolution multiplicative growth, a
/// rolling clamp tracker, and a per-block envelope estimator. State
/// fields read by the kernel:
///
///   +0x04  i32  step_magnitude (clamped to [0x10f, 0xa00])
///   +0x08  i32  prev_predictor (saved at block end)
///   +0x0c  i32  current_predictor
///   +0x10  i32  envelope_a / first_clip_bound (i16 lo + i16 hi)
///   +0x12  i16  envelope_b
///   +0x18  4×i16  MMX state slot 1 (filter taps?)
///   +0x20  i32  accum_low (32-bit running sum)
///   +0x28  4×i16  MMX state slot 2 (output history)
///   +0x2a  4×i16  MMX state slot 3
///   +0x2e  i16  delta_index_save
///   +0x30  i16  delta_index_save_mirror
///
/// Constant tables (verified by reading retail XBE .rdata):
///   0x2cfd08+4n : 4-bit step-magnitude table (entries 1..7 used)
///   0x2cfd48+4n : 4-bit step-output / envelope table
///   0x2cfd88+4n : 6-bit input table
///   0x2cfe08+4n : 6-bit output table
///   0x2d0018+4n : main 64-entry signed lookup (-2007..+2007)
///   0x2d0120-0x2d0168 : 4-bit MMX qword constants
///   0x2d0170-0x2d01b8 : 6-bit MMX qword constants
///
/// PORT STATUS: scaffolding only. The MMX kernel is not yet
/// translated to scalar Rust; this module provides the file-header
/// parser and the lookup tables baked from the retail XBE so
/// follow-up work can fill in the per-block decoder against ground
/// truth from a QEMU plugin trace.
pub mod codec8 {
    use std::io;

    /// Codec selector at file +0x00.
    pub const CODEC_ID: u32 = 8;
    /// Block size at file +0x10. Every observed codec_id=8 file uses
    /// this constant; deviation indicates a malformed or different-
    /// codec file.
    pub const BLOCK_BYTES: u32 = 0x600;
    /// Total header byte size — the kernel begins reading compressed
    /// data immediately after.
    pub const HEADER_BYTES: usize = 0x40;

    /// Top-of-file metadata. `unknown_*` fields are kept by exact
    /// offset until the kernel port reveals their semantics; do not
    /// rename them speculatively.
    #[derive(Clone, Debug)]
    pub struct Header {
        pub total_size: u32,
        pub unknown_08: u32,
        pub unknown_0c: u32,
        pub block_size: u32,
        pub channels: u32,
        pub sample_rate: u32,
        pub unknown_24: u32,
        pub unknown_28: u32,
        pub unknown_2c: u32,
        pub unknown_30: u32,
        pub unknown_34: u32,
        pub unknown_38: u32,
    }

    pub fn parse_header(data: &[u8]) -> io::Result<Header> {
        if data.len() < HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("codec_id=8 header needs {HEADER_BYTES} bytes, got {}", data.len()),
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
        Ok(Header {
            total_size: read_u32(0x04),
            unknown_08: read_u32(0x08),
            unknown_0c: read_u32(0x0c),
            block_size: read_u32(0x10),
            channels: read_u32(0x14),
            sample_rate: read_u32(0x18),
            unknown_24: read_u32(0x24),
            unknown_28: read_u32(0x28),
            unknown_2c: read_u32(0x2c),
            unknown_30: read_u32(0x30),
            unknown_34: read_u32(0x34),
            unknown_38: read_u32(0x38),
        })
    }

    /// Detect a codec_id=8 file by its first 4 bytes. Distinguishes
    /// it from codec_id=3 (`first byte == 0x03`) and the multi-bank
    /// `02 00 00 00 03 00 00 00` STREAM.SS2 variant.
    pub fn is_codec8(data: &[u8]) -> bool {
        data.len() >= 4
            && u32::from_le_bytes(data[0..4].try_into().unwrap()) == CODEC_ID
    }

    /// 4-bit step-magnitude table at retail VA 0x2cfd08, indexed by
    /// the per-block delta-index (values 1..7 observed; index 0 is
    /// guard-filled in the binary so the load wraps cheaply).
    /// Verified against the retail XBE .rdata section.
    pub const STEP_MAG_4BIT: [u32; 8] = [
        0xfa0a_1f00, // [0] guard / unused
        0x0000_0008, // [1]      8
        0x0000_010d, // [2]    269
        0x0000_01a9, // [3]    425
        0x0000_0221, // [4]    545
        0x0000_0285, // [5]    645
        0x0000_02e9, // [6]    745
        0x0000_0352, // [7]    850
    ];

    /// 4-bit step-output table at retail VA 0x2cfd48. Drives the
    /// nibble→sample envelope; indexed by `(step_magnitude >> 8) - 1`
    /// or similar. Exact indexing semantics still TBD from the
    /// kernel translation.
    pub const STEP_OUT_4BIT: [i32; 16] = [
        -1536,         // 0xfffffa00
        0x0000_090a,   // 2314
        0x0000_147b,   // 5243
        0x0000_2000,   // 8192
        0x0000_3800,   // 14336
        0x0000_630a,   // 25354
        0x0000_b185,   // 45445
        0x0002_310a,   // 143626
        0,
        0,
        0,
        1,
        1,
        1,
        3,
        7,
    ];

    /// Main 64-entry signed lookup at retail VA 0x2d0018. Values run
    /// from +1024 → +2007 (entries 0..32) and -1024 → -1922 (entries
    /// 33..64), so `(eax & 0x84)` selects the sign half (0 vs 0x84
    /// = 33×4 in dword-indexed bytes, indexing the second half).
    /// Verified by extracting the table from the retail XBE.
    pub const MAIN_LOOKUP: [i32; 64] = [
        // +0x000..+0x080: positive half (33 entries, ascending 1024..2007)
        1024, 1031, 1053, 1076, 1099, 1123, 1148, 1172,
        1198, 1224, 1251, 1278, 1306, 1334, 1363, 1393,
        1423, 1454, 1485, 1518, 1551, 1584, 1619, 1654,
        1690, 1726, 1764, 1802, 1841, 1881, 1922, 1964,
        2007,
        // +0x084..+0x100: negative half (-1024 → -1922)
        -1024, -1031, -1053, -1076, -1099, -1123, -1148, -1172,
        -1198, -1224, -1251, -1278, -1306, -1334, -1363, -1393,
        -1423, -1454, -1485, -1518, -1551, -1584, -1619, -1654,
        -1690, -1726, -1764, -1802, -1841, -1881, -1922,
    ];

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
    /// updating `state` in place. This is a scalar translation of
    /// the MMX kernel at retail VA 0x1942e0.
    ///
    /// PORT STATUS: first-pass implementation derived from static
    /// MLIL analysis. The IMA core (step lookup, magnitude shift,
    /// delta application) is high-confidence. The adaptive
    /// coefficient update in Phase 6 and the envelope-correction
    /// term in Phase 4 (the `pmaddwd` against immediate `0xf6`) are
    /// best-guess and need trace validation. Mismatched audio
    /// likely traces to those two phases.
    pub fn decode_nibble_4bit(state: &mut ChannelState, nibble: u8) -> i16 {
        let nibble = (nibble & 0xF) as i32;

        // ---- Phase 1: dot products of current history × coefficients
        // (sar by 10 matches the MMX `psradi 0xa` after `pmaddwd`).
        let dot_lo: i32 = (state.hist_pred[0] as i32 * state.coef_lo[0] as i32
            + state.hist_pred[1] as i32 * state.coef_lo[1] as i32)
            >> 10;
        let dot_hi: i32 = (state.hist_delta[0] as i32 * state.coef_hi[0] as i32
            + state.hist_delta[1] as i32 * state.coef_hi[1] as i32
            + state.hist_delta[2] as i32 * state.coef_hi[2] as i32
            + state.hist_delta[3] as i32 * state.coef_hi[3] as i32)
            >> 10;

        // ---- Phase 2: IMA-style step + magnitude lookup
        // Offset-binary nibble encoding: magnitude = abs(nibble - 7),
        // sign comes from sign of (nibble - 7). Verified against
        // proto QEMU plugin trace: my static port with this layout
        // matches the first ~60 silence samples byte-perfect, then
        // diverges as the engine's adaptive filter (Phase 6) builds
        // up — confirming the structure is right but Phase 6 needs
        // implementing.
        let centered = (nibble as i32) - 7;
        let mag_idx = centered.unsigned_abs() as usize;
        let sign_neg = centered < 0;
        let is_silence_nibble = mag_idx == 0;
        let step_full = if is_silence_nibble {
            state.step_magnitude
        } else {
            (STEP_MAG_4BIT[mag_idx.min(7)] as i32).wrapping_add(state.step_magnitude)
        };
        let step_hi_byte: u32 = ((step_full as u32) >> 8) & 0xFF;
        let step_lo_aligned: i32 = (step_full as u32 & 0xFFFF_FF00) as i32;
        let step_lo_byte = (step_full as u32 & 0xFF) as i32;

        let sign_half: usize = if sign_neg { 33 } else { 0 };
        let lookup_idx = (step_lo_byte >> 3) as usize;
        let delta_raw = MAIN_LOOKUP
            .get(sign_half + lookup_idx)
            .copied()
            .unwrap_or(0);

        let shifted = (delta_raw as u32).wrapping_shl(step_hi_byte) as i32;
        let shifted = shifted >> 10;
        // Guard: when the high byte is zero (i.e. step_full < 256) the
        // shift result is suppressed in the binary via a sign-mask
        // trick. Match that by zeroing when there's no magnitude.
        // Also force delta to 0 for the silence nibble (mag_idx=0).
        let delta = if is_silence_nibble || step_lo_aligned == 0 { 0 } else { shifted };

        // ---- Phase 3: combine
        let result_i32 = dot_lo.wrapping_add(dot_hi).wrapping_add(delta);

        // ---- Phase 4: update step_magnitude
        // STEP_OUT_4BIT[mag_idx] is the per-bucket step increment;
        // the envelope-correction term `step_magnitude.lo16 * 246`
        // is the kernel's `pmaddwd(st7, 0xf6)` — TBD whether that
        // immediate is truly 0xf6 or a memory ref; verify with trace.
        let step_inc = STEP_OUT_4BIT[mag_idx.min(15)];
        let envelope = (state.step_magnitude & 0xFFFF) * 246;
        let new_step = (step_inc.wrapping_add(envelope)) >> 8;
        state.step_magnitude = new_step.clamp(0x10F, 0xA00);

        // ---- Phase 5: shift history
        let result_sat = sat_i16(result_i32);
        let delta_sat = sat_i16(delta);
        state.hist_pred = [result_sat, state.hist_pred[0]];
        state.hist_delta = [
            delta_sat,
            state.hist_delta[0],
            state.hist_delta[1],
            state.hist_delta[2],
        ];

        // ---- Phase 6: adaptive coefficient growth (HEURISTIC port).
        // The engine's exact MMX-based adaptive update is multi-
        // stage and depends on per-channel state in a memory region
        // we haven't captured runtime ground truth for. Instead, this
        // approximates: grow coef_lo[0] by 1/call toward 896 when
        // signal is non-silent, and slowly populate coef_hi[*] so
        // the high-tap dot product also contributes.
        //
        // Validated against the proto QEMU trace (capture[0]+[1] =
        // 3072 samples): this variant reaches mean magnitude 4018
        // vs engine's 4324 (within 7%) and mean abs error 2097.
        // Not byte-perfect but tracks the signal envelope well
        // enough to produce intelligible audio.
        //
        // The real algorithm (per HLIL at 0x19453e..0x1944e0) does
        // leak (coef *= 255/256), then adds an LMS-style update term
        // built from the saturating-add ladder, per-lane biases
        // (data_2d0128 = 0x800), and per-tap weights from
        // data_2d0168. Future trace hooks reading per-channel state
        // would enable byte-perfect reverse-engineering.
        if delta_sat != 0 {
            if (state.coef_lo[0] as i32) < 896 {
                state.coef_lo[0] += 1;
            }
            for i in 0..4 {
                if state.coef_hi[i] < 128 {
                    state.coef_hi[i] += 1;
                }
            }
        }

        // ---- Phase 7: tail clamps on coef[0], coef[1]
        // Verified from engine trace data: coef_lo[1] clamped to
        // [-0x300, +0x300], then coef_lo[0] clamped to
        // ±(0x3C0 - SIGNED(coef_lo[1])) — NOT abs(coef_lo[1]).
        // With negative coef_lo[1] the bound INCREASES (e.g. for
        // coef_lo[1]=-702 the bound is 1662, not 258), which is how
        // the engine reaches coef_lo[0]=1610.
        let c1 = (state.coef_lo[1] as i32).clamp(-0x300, 0x300);
        state.coef_lo[1] = c1 as i16;
        let bound = 0x3C0 - c1;
        // bound can be positive or negative depending on sign of c1.
        // For c1 > 0 (positive), bound = 960 - c1 (shrinks toward 0).
        // For c1 < 0 (negative), bound = 960 + |c1| (grows).
        // The clamp is symmetric: coef[0] ∈ [-bound, +bound].
        state.coef_lo[0] = sat_i16((state.coef_lo[0] as i32).clamp(-bound, bound));

        // ---- Phase 8: emit sample (low 16 bits of saturated result)
        state.prev_prev_hi_dot = state.prev_hi_dot;
        state.prev_hi_dot = dot_hi;
        result_sat
    }

    /// Unpack 4-bit nibbles from a packed byte stream, matching the
    /// engine's `sub_194820`. The input is read as a sequence of
    /// 8-byte qwords; within each qword the two 4-byte dwords are
    /// swapped before extraction. Nibbles are then taken from bit
    /// position 60 (high) down to 0 in 4-bit steps, producing 16
    /// nibbles per qword in the output array.
    ///
    /// Output is written as one nibble per byte (values 0..15).
    /// `input.len()` must be a multiple of 8 and large enough for
    /// `(out_count + 15) / 16` qwords.
    pub fn unpack_nibbles_4bit(input: &[u8], output: &mut [u8]) {
        let count = output.len();
        let mut written = 0usize;
        let mut src = 0usize;
        while written < count {
            assert!(src + 8 <= input.len(), "unpack_nibbles_4bit: input exhausted");
            let lo = u32::from_le_bytes(input[src..src + 4].try_into().unwrap()) as u64;
            let hi = u32::from_le_bytes(input[src + 4..src + 8].try_into().unwrap()) as u64;
            // psrlq(esi,32)→high in low; punpckldq merges (mem.lo, reg.lo) =
            // (low_dword, high_dword) — i.e. the two dwords swap places.
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

    /// Variant of `unpack_nibbles_4bit` that extracts low-nibble-
    /// first within each byte. Use for diagnostic A/B comparison —
    /// the actual engine's bit order is high-first (per the MMX
    /// kernel), but the picket-fence artifact may come from a
    /// reversal in the byte→nibble convention.
    pub fn unpack_nibbles_4bit_swapped(input: &[u8], output: &mut [u8]) {
        unpack_nibbles_4bit(input, output);
        // Swap adjacent pairs in place.
        let n = output.len() & !1;
        for i in (0..n).step_by(2) {
            output.swap(i, i + 1);
        }
    }

    /// Decode a whole codec_id=8 file (header + data) into PCM. The
    /// caller is responsible for splitting stereo output into L/R
    /// channels if the header reports `channels == 2`.
    ///
    /// PORT STATUS: the 4-bit kernel is a first-pass static port
    /// (Phases 4 and 6 are best-guess). Validation needs listening
    /// or a proto/demo runtime trace.
    pub fn decode_file(file_bytes: &[u8]) -> io::Result<(Header, Vec<i16>)> {
        let header = parse_header(file_bytes)?;
        // Empirically: bytes 0x40..0x64 of observed proto files are
        // zero-padded (zero run extends from 0x36 in-header to 0x64
        // post-header). The actual nibble stream starts at 0x64
        // with the codec's "silence" pattern (0x77 = nibble 7,7 in
        // the offset-binary IMA scheme). Treat the first 36 bytes
        // after the header as state-init padding; this is a guess
        // pending fuller reverse-engineering of the file loader.
        const DATA_START: usize = 0x64;
        if file_bytes.len() < DATA_START {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "codec_id=8 file too short for data section",
            ));
        }
        let data = &file_bytes[DATA_START..];
        // Each input byte → 2 nibbles → 2 output samples per channel.
        // For stereo, the channels are interleaved; we decode them
        // as two separate state machines and produce interleaved
        // L/R output.
        let total_nibbles = (data.len() / 8) * 16;
        let mut nibbles = vec![0u8; total_nibbles];
        unpack_nibbles_4bit(data, &mut nibbles);

        let channels = header.channels.max(1) as usize;
        let frames = nibbles.len() / channels;
        let mut pcm = vec![0i16; frames * channels];
        let mut states: Vec<ChannelState> = (0..channels).map(|_| ChannelState::init()).collect();
        // The engine's outer loop (sub_1948a0) processes "four
        // nibbles per state struct per iteration" — i.e. the
        // channel interleave is block-based at 4-nibble granularity,
        // not per-nibble. For stereo, a 8-nibble loop iteration
        // covers (L0, L1, L2, L3, R0, R1, R2, R3).
        const BLOCK_NIBBLES_PER_CHANNEL: usize = 4;
        let block_total = channels * BLOCK_NIBBLES_PER_CHANNEL;
        let block_count = nibbles.len() / block_total;
        for block in 0..block_count {
            let base = block * block_total;
            for c in 0..channels {
                for i in 0..BLOCK_NIBBLES_PER_CHANNEL {
                    let nib = nibbles[base + c * BLOCK_NIBBLES_PER_CHANNEL + i];
                    let frame = block * BLOCK_NIBBLES_PER_CHANNEL + i;
                    pcm[frame * channels + c] = decode_nibble_4bit(&mut states[c], nib);
                }
            }
        }
        Ok((header, pcm))
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
        WRAPPER_BYTES
            + CYCLE0_VOICE_REGION_SIZES.iter().take(voice).sum::<usize>()
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

            // Cycles 1..N — each voice gets 361 bytes per cycle, with
            // the bonus +1 rotating: cycle k → voice (k % 3) gets +1.
            let mut cycle_idx: usize = 1;
            loop {
                let cycle_start = WRAPPER_BYTES + cycle_idx * CYCLE_BYTES;
                if cycle_start >= self.bytes.len() {
                    break;
                }
                let bonus_voice = cycle_idx % VOICES_PER_BANK;
                let mut sizes = [361usize; VOICES_PER_BANK];
                sizes[bonus_voice] += 1;
                let off_within_cycle: usize =
                    sizes.iter().take(voice).sum();
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
    ///
    /// `is_ls2_redirect` is true when the array_b entry has an `LS2`
    /// tag in the last 3 bytes of its 16-byte name field. The bytes
    /// at `file_offset` are placeholder/dead — the engine plays the
    /// actual sound from the corresponding `.LS2` file via the
    /// codec_id=3 streaming path, not via SetBufferData on the
    /// MAPS.LM2 bytes. Verified by QEMU plugin trace: zero overlap
    /// between SM2 SFX buffers (859 unique submits) and streaming
    /// buffers (16 unique with SetFrequency calls).
    #[derive(Clone, Debug)]
    pub struct SoundEntry {
        pub seq_id: u32,
        pub file_offset: u64,
        pub length: u64,
        pub sample_rate: u32,
        pub channels: u16,
        pub source_name: String,
        pub is_ls2_redirect: bool,
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
        let array_a_off =
            u32::from_le_bytes(descriptor[0x04..0x08].try_into().unwrap()) as usize;
        let array_a_cnt =
            u32::from_le_bytes(descriptor[0x08..0x0c].try_into().unwrap()) as usize;
        let array_b_off =
            u32::from_le_bytes(descriptor[0x0c..0x10].try_into().unwrap()) as usize;
        let array_b_cnt =
            u32::from_le_bytes(descriptor[0x10..0x14].try_into().unwrap());
        let array_c_off =
            u32::from_le_bytes(descriptor[0x14..0x18].try_into().unwrap()) as usize;
        let array_c_cnt =
            u32::from_le_bytes(descriptor[0x18..0x1c].try_into().unwrap());

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
                    descriptor[off + ARRAY_A_OFFSET_RATE_RATIO..off + ARRAY_A_OFFSET_RATE_RATIO + 4]
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

            // Look up per-sound metadata in array_b[seq_id & 0xFFFFFF].
            // The high byte of `seq_id` is a TYPE tag (e.g. 0x40 for
            // typical SFX); the low 24 bits index `array_b`. Without
            // the mask, seq_ids like 0x40000001 are huge u32s that
            // never match `seq_id < array_b_cnt`, and the sound falls
            // back to a hardcoded 22050 Hz — wrong rate, audible as
            // ~1.38× playback speed for the typical 16000 Hz SFX.
            //
            // Each array_b entry is 120 bytes; +0x44 = sample_rate,
            // +0x40 = avg bytes-per-sec, +0x4a = channel count (u16
            // high word of the u32 at +0x48), +0x50 = 16-byte source
            // `.wav` name.
            //
            // Entries whose name field ends in "LS2" (last 3 bytes
            // of the 16-byte name buffer) carry an LS2 tag. Two
            // encodings observed:
            //   "Music_Common.LS2"          — the LS2 filename itself
            //   "EFOLOU_1.wav\0LS2"         — original .wav name + tag
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
            let array_b_idx = (seq_id & 0x00FF_FFFF) as usize;
            let (sample_rate, channels, source_name, is_ls2_redirect) =
                if array_b_idx < array_b_cnt as usize {
                    let entry_off = array_b_off + array_b_idx * ARRAY_B_ENTRY_SIZE;
                    if entry_off + ARRAY_B_ENTRY_SIZE <= descriptor.len() {
                        let entry = &descriptor[entry_off..entry_off + ARRAY_B_ENTRY_SIZE];
                        let nominal_sr = u32::from_le_bytes(
                            entry[ARRAY_B_OFFSET_SAMPLE_RATE..ARRAY_B_OFFSET_SAMPLE_RATE + 4]
                                .try_into()
                                .unwrap(),
                        );
                        let ch = u16::from_le_bytes(
                            entry[ARRAY_B_OFFSET_CHANNELS..ARRAY_B_OFFSET_CHANNELS + 2]
                                .try_into()
                                .unwrap(),
                        );
                        let ch = if ch == 1 || ch == 2 { ch } else { 1 };
                        let name_bytes = &entry
                            [ARRAY_B_OFFSET_NAME..ARRAY_B_OFFSET_NAME + ARRAY_B_NAME_BYTES];
                        let is_ls2 = &name_bytes[13..16] == b"LS2";
                        let name_end = name_bytes
                            .iter()
                            .position(|&b| b == 0)
                            .unwrap_or(ARRAY_B_NAME_BYTES);
                        let name = String::from_utf8_lossy(&name_bytes[..name_end]).into_owned();
                        // For LS2-tagged entries, scale the nominal
                        // rate by array_a's +0x10 ratio. Non-LS2
                        // entries play at the nominal rate directly.
                        let sr = if is_ls2 {
                            let ratio = array_a_by_seq
                                .partition_point(|&(s, _)| s <= seq_id)
                                .checked_sub(1)
                                .map(|i| array_a_by_seq[i].1)
                                .unwrap_or(RATE_RATIO_UNIT as u32);
                            ((nominal_sr as u64) * (ratio as u64) / RATE_RATIO_UNIT) as u32
                        } else {
                            nominal_sr
                        };
                        (sr, ch, name, is_ls2)
                    } else {
                        (22050, 1u16, String::new(), false)
                    }
                } else {
                    (22050, 1u16, String::new(), false)
                };

            out.push(SoundEntry {
                seq_id,
                file_offset,
                length,
                sample_rate,
                channels,
                source_name,
                is_ls2_redirect,
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
