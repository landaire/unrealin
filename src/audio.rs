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
