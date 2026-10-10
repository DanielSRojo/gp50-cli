# gp50

Read, inspect, edit and write **Valeton GP-50** presets from the terminal, over
USB MIDI.

The protocol and `.prst` format come from
[drewmerc302/valeton-gp50](https://github.com/drewmerc302/valeton-gp50)
(MIT, `LICENSE.upstream`), ported to Rust from its Python code at commit
`aadcf9b`. Upstream reverse-engineered them and verified the write path against
Valeton Suite captures. The model and parameter catalog `src/fxid_ring.json` is
copied from it unchanged and embedded in the binary.

## Use

```bash
nix run . -- list                 # preset names (add -a for empty slots)
nix run . -- show 55              # decode a slot (or a .prst file)
nix run . -- pull 55 -o 55.prst   # read a slot into a file
nix run . -- edit 55.prst --set AMP.Gain=25 --name BluesClean --vol 60 -o 57.prst
nix run . -- push 57.prst 57      # write a slot
nix run . -- backup               # all 100 slots -> JSON
```

Slots are 0..99, as in the web editor. Blocks are `NR PRE DST AMP CAB EQ MOD DLY
RVB N->S`; `show` lists each block's parameter names for `--set`.

## Safety

- `push` refuses a non-empty slot without `--force`, backs up the target slot to
  `~/.local/state/gp50/backups/` first, asks for confirmation (skip with `-y`),
  then reads the slot back and compares it byte-for-byte.
- Reading a slot means selecting it with Program Change, so `pull`, `show N`,
  `push` and `backup` switch the active preset. Turn the volume down first.
- The pedal keeps only 10 characters of a preset name (the file has room for
  16); `edit --name` and `push` enforce that.
- Only the read requests and the validated write stream are ever sent. Upstream
  wedged the pedal once with guessed traffic; don't add new opcodes without a
  Suite capture to check them against.

## Develop

```bash
nix develop
cargo test     # offline: format decoding/edits + write stream vs upstream's builder
cargo clippy --all-targets
```

`tests/stream-55-slot57.hex` is the write stream upstream's Python builder
produces for `tests/55-Blues_Edge.prst` to slot 57; the Rust stream must match it
byte-for-byte. MIDI goes through `midir` (ALSA on Linux, CoreMIDI on macOS).
