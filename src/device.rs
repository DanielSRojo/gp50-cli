//! GP-50 SysEx protocol over USB MIDI: read names/bodies, select, write.
//!
//! Adapted from drewmerc302/valeton-gp50 (patch/live_read.py, scan_bank.py,
//! select_patch.py, device_write.py; MIT, see LICENSE.upstream). Protocol notes
//! are in that repo's re/DEVICE_READ.md and re/DEVICE_WRITE.md.
//!
//! Every packet: BUF = [crc, cmd, index, length, *payload], crc = CRC-8/0x07
//! over BUF with the crc byte held 0; on the wire BUF is nibble-expanded (hi
//! first) inside F0..F7.
//!
//! Safety rules from upstream (the pedal has a shallow MIDI queue and has
//! wedged on bad traffic): one persistent port, one request at a time, settle
//! between requests, and only ever send the read requests and the validated
//! write stream below.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, channel};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};

use crate::prst;

const READ_CMD: u8 = 0x01;
const CATSEL: u8 = 0x12;
const SEL_NAMES: u8 = 0x40;
/// Body of the *active* preset; select with Program Change first.
const SEL_BODY: u8 = 0x41;
/// Settle after Program Change (0.15 s races, 0.25 s is clean).
const POST_PC: Duration = Duration::from_millis(300);
const SETTLE: Duration = Duration::from_millis(200);

const WRITE_CMD: u8 = 0x1D;
const WRITE_BLOCK: usize = 19;
const WRITE_HDR: [u8; 2] = [0x11, 0x4F];
const ACK_WAIT: Duration = Duration::from_millis(150);

const PORT_MATCH: &str = "GP-50";

/// One packet's wire bytes, including F0/F7.
fn packet(cmd: u8, index: u8, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0, cmd, index, payload.len() as u8];
    buf.extend_from_slice(payload);
    buf[0] = prst::crc8(&buf);
    let mut wire = vec![0xF0];
    for b in buf {
        wire.extend([b >> 4, b & 0x0F]);
    }
    wire.push(0xF7);
    wire
}

fn nib_decode(mid: &[u8]) -> Vec<u8> {
    mid.as_chunks::<2>()
        .0
        .iter()
        .map(|[hi, lo]| hi << 4 | lo)
        .collect()
}

fn write_payload(prst: &[u8], slot: u8) -> Vec<u8> {
    let mut payload = WRITE_HDR.to_vec();
    payload.extend([slot, 0, 0, 0]);
    payload.extend_from_slice(&prst[prst::NAME_OFF..]);
    payload
}

/// Suite's exact patch-import stream for `prst` -> `slot` (0-based):
/// payload = [11 4F slot 00 00 00] + prst[0x19..], sent as cmd 0x1D in
/// 19-byte blocks. Upstream verified it byte-for-byte against Suite captures.
pub fn write_stream(prst: &[u8], slot: u8) -> Result<Vec<Vec<u8>>> {
    if slot > 99 {
        bail!("slot {slot} out of range 0..99");
    }
    prst::check(prst)?;
    Ok(write_payload(prst, slot)
        .chunks(WRITE_BLOCK)
        .enumerate()
        .map(|(i, chunk)| packet(WRITE_CMD, i as u8, chunk))
        .collect())
}

/// Decode the stream back and check framing, CRCs, indices and that the
/// payload is exactly header + slot + the .prst we meant to send.
pub fn validate_stream(packets: &[Vec<u8>], slot: u8, prst: &[u8]) -> Result<()> {
    let mut payload = Vec::new();
    for (i, wire) in packets.iter().enumerate() {
        if wire.first() != Some(&0xF0) || wire.last() != Some(&0xF7) {
            bail!("packet {i}: not F0..F7 framed");
        }
        let buf = nib_decode(&wire[1..wire.len() - 1]);
        if buf.len() < 4 {
            bail!("packet {i}: truncated");
        }
        let mut zeroed = buf.clone();
        zeroed[0] = 0;
        if prst::crc8(&zeroed) != buf[0] {
            bail!("packet {i}: bad CRC");
        }
        if buf[1] != WRITE_CMD || buf[2] as usize != i || buf[3] as usize != buf.len() - 4 {
            bail!("packet {i}: bad header");
        }
        payload.extend_from_slice(&buf[4..]);
    }
    if payload != write_payload(prst, slot) {
        bail!("reassembled payload does not match the .prst");
    }
    Ok(())
}

/// One persistent in/out connection for the whole session.
pub struct Gp50 {
    out: MidiOutputConnection,
    _input: MidiInputConnection<()>,
    rx: Receiver<Vec<u8>>,
    /// A SysEx frame the backend delivered in pieces.
    partial: Vec<u8>,
}

impl Gp50 {
    pub fn open() -> Result<Self> {
        let mut input = MidiInput::new("gp50").context("opening MIDI input")?;
        input.ignore(Ignore::None); // SysEx is filtered out by default
        let in_port = input
            .ports()
            .into_iter()
            .find(|p| input.port_name(p).is_ok_and(|n| n.contains(PORT_MATCH)))
            .ok_or_else(not_found)?;
        let output = MidiOutput::new("gp50").context("opening MIDI output")?;
        let out_port = output
            .ports()
            .into_iter()
            .find(|p| output.port_name(p).is_ok_and(|n| n.contains(PORT_MATCH)))
            .ok_or_else(not_found)?;

        let (tx, rx) = channel();
        let input = input
            .connect(
                &in_port,
                "gp50-in",
                move |_, msg, _| drop(tx.send(msg.to_vec())),
                (),
            )
            .map_err(|e| anyhow!("connecting to GP-50 input: {e}"))?;
        let out = output
            .connect(&out_port, "gp50-out")
            .map_err(|e| anyhow!("connecting to GP-50 output: {e}"))?;
        let mut dev = Gp50 {
            out,
            _input: input,
            rx,
            partial: Vec::new(),
        };
        sleep(SETTLE);
        dev.drain();
        Ok(dev)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.out
            .send(bytes)
            .map_err(|e| anyhow!("MIDI send failed: {e}"))
    }

    fn drain(&mut self) {
        while self.rx.try_recv().is_ok() {}
        self.partial.clear();
    }

    /// Next complete SysEx frame (F0..F7) within `timeout`, if any.
    fn recv_frame(&mut self, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            let chunk = self.rx.recv_timeout(left).ok()?;
            if chunk.first() == Some(&0xF0) {
                self.partial.clear();
            } else if self.partial.is_empty() {
                continue; // not SysEx (or a stray continuation)
            }
            self.partial.extend_from_slice(&chunk);
            if self.partial.last() == Some(&0xF7) {
                return Some(std::mem::take(&mut self.partial));
            }
        }
    }

    fn read(&mut self, selector: u8) -> Result<Vec<u8>> {
        const WAIT: Duration = Duration::from_millis(2500);
        const IDLE: Duration = Duration::from_millis(150);
        let mut req = [0, READ_CMD, 0x00, 0x02, CATSEL, selector];
        req[0] = prst::crc8(&req);
        let mut wire = vec![0xF0];
        for b in req {
            wire.extend([b >> 4, b & 0x0F]);
        }
        wire.push(0xF7);

        self.drain();
        self.send(&wire)?;
        let (t0, mut last) = (Instant::now(), Instant::now());
        let mut replies = Vec::new();
        while t0.elapsed() < WAIT {
            match self.recv_frame(Duration::from_millis(10)) {
                Some(frame) => {
                    replies.push(nib_decode(&frame[1..frame.len() - 1]));
                    last = Instant::now();
                }
                None if !replies.is_empty() && last.elapsed() > IDLE => break,
                None => {}
            }
        }
        // group chunks by reply cmd, order by chunk index, keep the longest stream
        let mut by_cmd: BTreeMap<u8, Vec<(u8, Vec<u8>)>> = BTreeMap::new();
        for r in replies.into_iter().filter(|r| r.len() >= 4) {
            by_cmd
                .entry(r[1])
                .or_default()
                .push((r[2], r[4..].to_vec()));
        }
        let blob = by_cmd
            .into_values()
            .map(|mut chunks| {
                chunks.sort_by_key(|(idx, _)| *idx);
                chunks.into_iter().flat_map(|(_, d)| d).collect::<Vec<u8>>()
            })
            .max_by_key(Vec::len)
            .unwrap_or_default();
        sleep(SETTLE);
        Ok(blob)
    }

    /// All 100 preset names, by slot.
    pub fn names(&mut self) -> Result<HashMap<u8, String>> {
        let blob = self.read(SEL_NAMES)?;
        // 2-byte selector echo, then [u32le index][16-byte name] records
        let names: HashMap<u8, String> = blob
            .get(2..)
            .unwrap_or_default()
            .as_chunks::<20>()
            .0
            .iter()
            .map(|rec| {
                let idx = u32::from_le_bytes(rec[..4].try_into().unwrap()) as u8;
                let raw = &rec[4..];
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                (
                    idx,
                    raw[..end]
                        .iter()
                        .map(|&b| b as char)
                        .collect::<String>()
                        .trim()
                        .to_string(),
                )
            })
            .collect();
        if names.len() != 100 {
            bail!("name read returned {} names, expected 100", names.len());
        }
        Ok(names)
    }

    pub fn select(&mut self, slot: u8) -> Result<()> {
        self.send(&[0xC0, slot & 0x7F])?;
        sleep(POST_PC);
        Ok(())
    }

    /// Select `slot` (this changes the active preset!) and read it back.
    pub fn read_slot(&mut self, slot: u8, name: Option<&str>) -> Result<Vec<u8>> {
        let name = match name {
            Some(n) => n.to_string(),
            None => self.names()?.remove(&slot).unwrap_or_default(),
        };
        self.select(slot)?;
        let mut len = 0;
        for _ in 0..2 {
            let blob = self.read(SEL_BODY)?;
            let body = blob.strip_prefix(&[CATSEL, SEL_BODY][..]).unwrap_or(&blob);
            if body.len() == prst::BODY_LEN {
                return prst::rebuild(&name, body);
            }
            len = body.len();
            sleep(Duration::from_millis(400));
        }
        bail!("slot {slot}: body read came back {len} bytes")
    }

    /// Send a validated write stream, pacing on the per-block ACK.
    /// Returns (packets sent, ACKs seen); the two should be equal.
    pub fn write_slot(&mut self, slot: u8, prst: &[u8]) -> Result<(usize, usize)> {
        let packets = write_stream(prst, slot)?;
        validate_stream(&packets, slot, prst)?;
        self.drain();
        let mut acks = 0;
        for wire in &packets {
            self.send(wire)?;
            if self.recv_frame(ACK_WAIT).is_some() {
                acks += 1;
            }
        }
        sleep(Duration::from_millis(500));
        Ok((packets.len(), acks))
    }
}

fn not_found() -> anyhow::Error {
    anyhow!("GP-50 not found on USB MIDI. Check the cable carries data and the pedal is on.")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLUES_EDGE: &[u8] = include_bytes!("../tests/55-Blues_Edge.prst");
    /// Built by upstream's `build_patch_write_stream` (validated against Suite captures).
    const UPSTREAM_STREAM: &str = include_str!("../tests/stream-55-slot57.hex");

    #[test]
    fn write_stream_matches_upstream() {
        let ours = write_stream(BLUES_EDGE, 57).unwrap();
        let theirs: Vec<Vec<u8>> = UPSTREAM_STREAM
            .lines()
            .map(|l| {
                l.split(' ')
                    .map(|h| u8::from_str_radix(h, 16).unwrap())
                    .collect()
            })
            .collect();
        assert_eq!(ours.len(), 29);
        assert_eq!(ours, theirs);
        validate_stream(&ours, 57, BLUES_EDGE).unwrap();
    }

    #[test]
    fn validate_catches_corruption() {
        let mut packets = write_stream(BLUES_EDGE, 57).unwrap();
        assert!(validate_stream(&packets, 56, BLUES_EDGE).is_err());
        packets[3][10] ^= 0x01;
        assert!(validate_stream(&packets, 57, BLUES_EDGE).is_err());
    }
}
