//! The 552-byte GP-50 .prst format: layout, CRC, decoding and edits.
//!
//! Adapted from drewmerc302/valeton-gp50 (patch/prst_format.py and
//! app/patchlib.py, MIT, see LICENSE.upstream). GP-50 only.
//!
//! ```text
//! prst[0x00..0x14]  constant "GP-50" header
//! prst[0x14]        file CRC: CRC-8/0x07 over prst[0x15..]
//! prst[0x15..0x19]  FF FF FF FF sentinel
//! prst[0x19..0x29]  16-byte patch name, latin1, NUL-padded
//! prst[0x29..]      511-byte body
//! ```
//!
//! Body records are found by magic, not fixed offset:
//! - `REC_MODELS`  10 x [fxlow b0][b1][b2][category]
//! - `REC_BYPASS`  u32, bit k = block k active
//! - `REC_PARAMS`  80 x f32 (10 blocks x 8 param slots)
//! - `FS_TRAILER`  [FS1 u32][FS2 u32] block bitmasks

use anyhow::{Result, bail};

use crate::catalog;

pub const PRST_LEN: usize = 552;
pub const BODY_LEN: usize = 511;
const HEADER: [u8; 20] = *b"GP-50\0\0\0\0\0\0\0\0\0\0\0\0\0\x01\0";
pub const CRC_OFF: usize = 0x14;
const SENTINEL: [u8; 4] = [0xFF; 4];
pub const NAME_OFF: usize = 0x19;
const NAME_LEN: usize = 16;
/// The pedal keeps only the first 10 characters of a name.
pub const NAME_MAX: usize = 10;
pub const BODY_OFF: usize = 0x29;
/// Factory-blank slots carry the device name.
const EMPTY_NAME: &str = "GP-50";

const REC_MODELS: [u8; 4] = [0x03, 0x30, 0x28, 0x00];
const REC_BYPASS: [u8; 4] = [0x01, 0x30, 0x04, 0x00];
const REC_PARAMS: [u8; 4] = [0x04, 0x30, 0x40, 0x01];
const FS_TRAILER: [u8; 4] = [0x03, 0x00, 0x0A, 0x00];
/// First group-0x20 patch-settings record.
const SETTINGS_OFF: usize = 0x55;

pub const BLOCKS: [&str; 10] = [
    "NR", "PRE", "DST", "AMP", "CAB", "EQ", "MOD", "DLY", "RVB", "N->S",
];
const NS_CAT: u32 = 0x0F;

/// CRC-8, poly 0x07, no reflection, no final XOR.
pub fn crc8(data: &[u8]) -> u8 {
    let mut c = 0u8;
    for &b in data {
        c ^= b;
        for _ in 0..8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
        }
    }
    c
}

pub fn check(prst: &[u8]) -> Result<()> {
    if prst.len() != PRST_LEN || prst[..HEADER.len()] != HEADER {
        bail!("not a GP-50 .prst ({} bytes)", prst.len());
    }
    if prst[CRC_OFF] != crc8(&prst[CRC_OFF + 1..]) {
        bail!("bad .prst CRC");
    }
    Ok(())
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

pub fn read_name(prst: &[u8]) -> String {
    let raw = &prst[NAME_OFF..BODY_OFF];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    latin1(&raw[..end]).trim().to_string()
}

pub fn is_empty(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(EMPTY_NAME)
}

/// Full .prst from a device read: name (selector 0x40) + body (0x41).
pub fn rebuild(name: &str, body: &[u8]) -> Result<Vec<u8>> {
    if body.len() != BODY_LEN {
        bail!("expected a {BODY_LEN}-byte body, got {}", body.len());
    }
    let mut out = Vec::with_capacity(PRST_LEN);
    out.extend_from_slice(&HEADER);
    out.push(0);
    out.extend_from_slice(&SENTINEL);
    out.extend_from_slice(&[0; NAME_LEN]);
    out.extend_from_slice(body);
    write_name(&mut out, name);
    refix_crc(&mut out);
    Ok(out)
}

fn encode_latin1(name: &str) -> Vec<u8> {
    name.chars()
        .map(|c| u8::try_from(c).unwrap_or(b'?'))
        .collect()
}

fn write_name(b: &mut [u8], name: &str) {
    let mut enc = encode_latin1(name);
    enc.resize(NAME_LEN, 0);
    b[NAME_OFF..BODY_OFF].copy_from_slice(&enc);
}

fn refix_crc(b: &mut [u8]) {
    b[CRC_OFF] = crc8(&b[CRC_OFF + 1..]);
}

fn after(b: &[u8], magic: &[u8; 4], last: bool) -> Result<usize> {
    let mut hits = b
        .windows(magic.len())
        .enumerate()
        .filter(|(_, w)| w == magic);
    let hit = if last { hits.next_back() } else { hits.next() };
    match hit {
        Some((i, _)) => Ok(i + magic.len()),
        None => bail!("record {} not found", hex(magic)),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn f32_at(b: &[u8], i: usize) -> f32 {
    f32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

// --- decoding ----------------------------------------------------------------------

pub struct Param {
    pub name: String,
    /// Index into the 80-float array.
    pub slot: usize,
    pub value: f32,
    pub unit: String,
    pub toggle: bool,
    pub lo: f64,
    pub hi: f64,
}

impl Param {
    pub fn display(&self) -> String {
        if self.toggle {
            return if self.value.round() != 0.0 {
                "On"
            } else {
                "Off"
            }
            .into();
        }
        let v = self.value as f64;
        let s = if (v - v.round()).abs() < 1e-4 {
            format!("{}", v.round() as i64)
        } else {
            format!("{v:.2}")
        };
        if self.unit.is_empty() {
            s
        } else {
            format!("{s} {}", self.unit)
        }
    }
}

pub struct Block {
    pub name: &'static str,
    pub active: bool,
    pub model: Option<String>,
    pub origin: Option<String>,
    pub params: Vec<Param>,
}

pub struct Patch {
    pub name: String,
    pub blocks: Vec<Block>,
    pub patch_vol: Option<i64>,
    pub fs1: Vec<&'static str>,
    pub fs2: Vec<&'static str>,
}

impl Patch {
    pub fn block(&self, name: &str) -> Result<&Block> {
        match self
            .blocks
            .iter()
            .find(|b| b.name.eq_ignore_ascii_case(name))
        {
            Some(b) => Ok(b),
            None => bail!("no block {name:?} (blocks: {})", BLOCKS.join(", ")),
        }
    }
}

pub fn decode(prst: &[u8]) -> Result<Patch> {
    check(prst)?;
    let mods = after(prst, &REC_MODELS, false)?;
    let mask = u32_at(prst, after(prst, &REC_BYPASS, false)?);
    let pf = after(prst, &REC_PARAMS, false)?;
    let mut blocks = Vec::with_capacity(BLOCKS.len());
    for (k, &bname) in BLOCKS.iter().enumerate() {
        let r = &prst[mods + k * 4..mods + k * 4 + 4];
        let fxlow = r[0] as u32 | (r[1] as u32) << 8 | (r[2] as u32) << 16;
        let fxid = if bname == "N->S" {
            NS_CAT << 24 | r[0] as u32
        } else {
            (r[3] as u32) << 24 | fxlow
        };
        let entry = catalog::get(fxid);
        let params = entry
            .map(|e| {
                e.params
                    .iter()
                    .map(|p| Param {
                        name: p.name.clone(),
                        slot: k * 8 + p.alg_id,
                        value: f32_at(prst, pf + (k * 8 + p.alg_id) * 4),
                        unit: p.unit.clone(),
                        toggle: p.toggle,
                        lo: p.min,
                        hi: p.max,
                    })
                    .collect()
            })
            .unwrap_or_default();
        blocks.push(Block {
            name: bname,
            active: mask >> k & 1 == 1,
            model: entry.and_then(|e| e.model_name()),
            origin: entry
                .and_then(|e| e.origin.clone())
                .filter(|o| !o.is_empty()),
            params,
        });
    }
    let fs = after(prst, &FS_TRAILER, true)?;
    let (fs1, fs2) = (u32_at(prst, fs), u32_at(prst, fs + 4));
    let pick = |m: u32| {
        (0..10)
            .filter(|i| m >> i & 1 == 1)
            .map(|i| BLOCKS[i])
            .collect()
    };
    Ok(Patch {
        name: read_name(prst),
        blocks,
        patch_vol: settings(prst)
            .into_iter()
            .find(|(id, _)| *id == 0x01)
            .map(|(_, v)| v),
        fs1: pick(fs1),
        fs2: pick(fs2),
    })
}

/// Group-0x20 patch-settings records as (id, offset of value, length).
/// id 1 = patch volume, id 2 = BPM.
fn settings_records(b: &[u8]) -> Vec<(u8, usize, usize)> {
    let mut out = Vec::new();
    let mut i = SETTINGS_OFF;
    while i + 4 <= b.len() && b[i + 1] == 0x20 {
        let ln = u16::from_le_bytes([b[i + 2], b[i + 3]]) as usize;
        if !matches!(ln, 1 | 2 | 4) {
            break;
        }
        out.push((b[i], i + 4, ln));
        i += 4 + ln;
    }
    out
}

fn settings(b: &[u8]) -> Vec<(u8, i64)> {
    settings_records(b)
        .into_iter()
        .map(|(id, off, ln)| {
            let raw = &b[off..off + ln];
            // little-endian, sign-extended
            let mut v: i64 = if raw[ln - 1] & 0x80 != 0 { -1 } else { 0 };
            for &x in raw.iter().rev() {
                v = (v << 8) | x as i64;
            }
            (id, v)
        })
        .collect()
}

// --- edits -------------------------------------------------------------------------

pub fn set_param(prst: &[u8], block: &str, param: &str, value: f64) -> Result<Vec<u8>> {
    let p = decode(prst)?;
    let blk = p.block(block)?;
    let Some(prm) = blk
        .params
        .iter()
        .find(|x| x.name.eq_ignore_ascii_case(param))
    else {
        let names: Vec<_> = blk.params.iter().map(|x| x.name.as_str()).collect();
        bail!(
            "{} ({}) has no param {param:?} (params: {})",
            blk.name,
            blk.model.as_deref().unwrap_or("?"),
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        );
    };
    if !(prm.lo..=prm.hi).contains(&value) {
        bail!(
            "{}.{} must be {}..{}, got {value}",
            blk.name,
            prm.name,
            prm.lo,
            prm.hi
        );
    }
    let mut b = prst.to_vec();
    let at = after(&b, &REC_PARAMS, false)? + prm.slot * 4;
    b[at..at + 4].copy_from_slice(&(value as f32).to_le_bytes());
    refix_crc(&mut b);
    Ok(b)
}

pub fn set_name(prst: &[u8], name: &str) -> Result<Vec<u8>> {
    if name.chars().count() > NAME_MAX {
        bail!("the GP-50 keeps at most {NAME_MAX} characters: {name:?}");
    }
    let mut b = prst.to_vec();
    write_name(&mut b, name);
    refix_crc(&mut b);
    Ok(b)
}

pub fn set_patch_vol(prst: &[u8], vol: u8) -> Result<Vec<u8>> {
    if vol > 100 {
        bail!("patch volume must be 0..100, got {vol}");
    }
    let mut b = prst.to_vec();
    let Some((_, off, ln)) = settings_records(&b)
        .into_iter()
        .find(|(id, _, _)| *id == 0x01)
    else {
        bail!("patch volume record not found");
    };
    b[off..off + ln].fill(0);
    b[off] = vol;
    refix_crc(&mut b);
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLUES_EDGE: &[u8] = include_bytes!("../tests/55-Blues_Edge.prst");

    fn value(p: &Patch, block: &str, param: &str) -> f32 {
        let b = p.block(block).unwrap();
        b.params.iter().find(|x| x.name == param).unwrap().value
    }

    #[test]
    fn decode_matches_notes() {
        let p = decode(BLUES_EDGE).unwrap();
        assert_eq!(p.name, "Blues Edge");
        let amp = p.block("AMP").unwrap();
        assert!(amp.active);
        assert_eq!(amp.model.as_deref(), Some("Bellman 59N"));
        assert_eq!(value(&p, "AMP", "Gain"), 55.0);
        assert_eq!(p.fs1, ["DST"]);
        assert_eq!(p.patch_vol, Some(50));
    }

    #[test]
    fn rebuild_roundtrip() {
        assert_eq!(
            rebuild("Blues Edge", &BLUES_EDGE[BODY_OFF..]).unwrap(),
            BLUES_EDGE
        );
    }

    #[test]
    fn set_param_only_touches_that_float_and_crc() {
        let out = set_param(BLUES_EDGE, "amp", "gain", 25.0).unwrap();
        check(&out).unwrap();
        assert_eq!(value(&decode(&out).unwrap(), "AMP", "Gain"), 25.0);
        let changed: Vec<_> = (0..PRST_LEN).filter(|&i| BLUES_EDGE[i] != out[i]).collect();
        assert!(
            changed.len() <= 5 && changed.contains(&CRC_OFF),
            "{changed:?}"
        );
    }

    #[test]
    fn set_param_rejects_out_of_range_and_unknown() {
        assert!(set_param(BLUES_EDGE, "AMP", "Gain", 150.0).is_err());
        assert!(set_param(BLUES_EDGE, "AMP", "Drive", 10.0).is_err());
        assert!(set_param(BLUES_EDGE, "XYZ", "Gain", 10.0).is_err());
    }

    #[test]
    fn name_and_volume() {
        let out = set_patch_vol(&set_name(BLUES_EDGE, "BluesClean").unwrap(), 60).unwrap();
        let p = decode(&out).unwrap();
        assert_eq!((p.name.as_str(), p.patch_vol), ("BluesClean", Some(60)));
        assert!(set_name(BLUES_EDGE, "Blues Clean").is_err()); // 11 > 10 the pedal keeps
    }
}
