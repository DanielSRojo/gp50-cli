//! gp50: read, inspect, edit and write Valeton GP-50 presets from the terminal.
//!
//! Slots are 0..99, the same numbering as the web editor.

mod catalog;
mod device;
mod prst;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use base64::Engine;
use clap::{Parser, Subcommand};

use device::Gp50;

#[derive(Parser)]
#[command(
    name = "gp50",
    version,
    about = "Read, inspect, edit and write Valeton GP-50 presets"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List preset names on the pedal
    List {
        /// Include empty slots
        #[arg(short, long)]
        all: bool,
    },
    /// Read a slot into a .prst file
    Pull {
        #[arg(value_parser = slot)]
        slot: u8,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Decode a .prst file or a slot on the pedal
    Show {
        /// .prst path or slot number
        source: String,
        /// Include bypassed blocks
        #[arg(short, long)]
        all: bool,
    },
    /// Edit a .prst (or a slot) into a new file, offline
    Edit {
        /// .prst path or slot number
        source: String,
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// e.g. AMP.Gain=25 (repeatable)
        #[arg(long = "set", value_name = "BLOCK.PARAM=VALUE")]
        set: Vec<String>,
        /// New preset name (max 10 chars)
        #[arg(long)]
        name: Option<String>,
        /// Patch volume 0..100
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
        vol: Option<u8>,
    },
    /// Write a .prst to a slot (backs up, then verifies)
    Push {
        file: PathBuf,
        #[arg(value_parser = slot)]
        slot: u8,
        /// Allow overwriting a non-empty slot
        #[arg(long)]
        force: bool,
        /// Don't ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Read all 100 slots into a JSON backup
    Backup {
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

fn slot(s: &str) -> Result<u8, String> {
    match s.parse::<u8>() {
        Ok(n) if n <= 99 => Ok(n),
        _ => Err(format!("slot must be 0..99, got {s:?}")),
    }
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        })
        .join("gp50")
}

fn volume_warning(slot: u8) {
    eprintln!("note: this switches the pedal to slot {slot}; mind your IEM volume.");
}

fn safe(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn now(fmt: &str) -> String {
    chrono::Local::now().format(fmt).to_string()
}

fn read_prst(path: &Path) -> Result<Vec<u8>> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    prst::check(&data).with_context(|| path.display().to_string())?;
    Ok(data)
}

/// A .prst path, or a slot number to read from the pedal.
fn load(src: &str) -> Result<Vec<u8>> {
    if !src.is_empty() && src.bytes().all(|b| b.is_ascii_digit()) {
        let n = slot(src).map_err(anyhow::Error::msg)?;
        volume_warning(n);
        return Gp50::open()?.read_slot(n, None);
    }
    read_prst(Path::new(src))
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    fs::write(path, data).with_context(|| format!("writing {}", path.display()))
}

fn cmd_list(all: bool) -> Result<()> {
    let names = Gp50::open()?.names()?;
    for n in 0..100u8 {
        let name = &names[&n];
        let empty = prst::is_empty(name);
        if all || !empty {
            println!("{n:2}  {name}{}", if empty { "  (empty)" } else { "" });
        }
    }
    Ok(())
}

fn cmd_pull(n: u8, output: Option<PathBuf>) -> Result<()> {
    let data = load(&n.to_string())?;
    let name = prst::read_name(&data);
    let out = output.unwrap_or_else(|| format!("{n:02}-{}.prst", safe(&name)).into());
    write(&out, &data)?;
    println!("slot {n} ({name}) -> {}", out.display());
    Ok(())
}

fn cmd_show(source: &str, all: bool) -> Result<()> {
    let p = prst::decode(&load(source)?)?;
    let fs = |v: &[&str]| {
        if v.is_empty() {
            "-".to_string()
        } else {
            v.join("+")
        }
    };
    let vol = p.patch_vol.map_or("None".into(), |v| v.to_string());
    println!(
        "{}   patch vol {vol}   FS1 {}   FS2 {}",
        p.name,
        fs(&p.fs1),
        fs(&p.fs2)
    );
    for b in &p.blocks {
        if !all && !b.active && !p.fs1.contains(&b.name) && !p.fs2.contains(&b.name) {
            continue;
        }
        let origin = b
            .origin
            .as_ref()
            .map(|o| format!(" ({o})"))
            .unwrap_or_default();
        let state = if b.active { "on " } else { "off" };
        println!(
            "  {:<4} {state} {}{origin}",
            b.name,
            b.model.as_deref().unwrap_or("?")
        );
        if !b.params.is_empty() {
            let params: Vec<_> = b
                .params
                .iter()
                .map(|x| format!("{} {}", x.name, x.display()))
                .collect();
            println!("            {}", params.join("  "));
        }
    }
    Ok(())
}

fn diff(before: &[u8], after: &[u8]) -> Result<Vec<String>> {
    let (pb, pa) = (prst::decode(before)?, prst::decode(after)?);
    let mut out = Vec::new();
    if pb.name != pa.name {
        out.push(format!("name: {} -> {}", pb.name, pa.name));
    }
    if pb.patch_vol != pa.patch_vol {
        let v = |x: Option<i64>| x.map_or("None".into(), |v| v.to_string());
        out.push(format!(
            "patch vol: {} -> {}",
            v(pb.patch_vol),
            v(pa.patch_vol)
        ));
    }
    for (bb, ba) in pb.blocks.iter().zip(&pa.blocks) {
        for (xb, xa) in bb.params.iter().zip(&ba.params) {
            if xb.value != xa.value {
                out.push(format!(
                    "{}.{}: {} -> {}",
                    ba.name,
                    xa.name,
                    xb.display(),
                    xa.display()
                ));
            }
        }
    }
    Ok(out)
}

fn cmd_edit(
    source: &str,
    output: Option<PathBuf>,
    sets: &[String],
    name: Option<String>,
    vol: Option<u8>,
) -> Result<()> {
    let before = load(source)?;
    let mut data = before.clone();
    for spec in sets {
        let parsed = spec.split_once('=').and_then(|(key, value)| {
            let (block, param) = key.split_once('.')?;
            Some((block, param, value.parse::<f64>().ok()?))
        });
        let Some((block, param, value)) = parsed.filter(|(b, p, _)| !b.is_empty() && !p.is_empty())
        else {
            bail!("--set expects BLOCK.PARAM=VALUE, got {spec:?}");
        };
        data = prst::set_param(&data, block, param, value)?;
    }
    if let Some(v) = vol {
        data = prst::set_patch_vol(&data, v)?;
    }
    if let Some(n) = &name {
        data = prst::set_name(&data, n)?;
    }
    let changes = diff(&before, &data)?;
    let out = output.unwrap_or_else(|| format!("{}.prst", safe(&prst::read_name(&data))).into());
    write(&out, &data)?;
    if changes.is_empty() {
        println!("  (no changes)");
    }
    for c in changes {
        println!("  {c}");
    }
    println!("-> {}", out.display());
    Ok(())
}

fn cmd_push(file: &Path, n: u8, force: bool, yes: bool) -> Result<()> {
    let data = read_prst(file)?;
    let name = prst::read_name(&data);
    if name.chars().count() > prst::NAME_MAX {
        bail!(
            "name {name:?} is longer than the pedal's {} characters; rename it first: \
             gp50 edit {f} --name ... -o {f}",
            prst::NAME_MAX,
            f = file.display()
        );
    }
    device::write_stream(&data, n)?; // validate before touching the pedal

    let mut dev = Gp50::open()?;
    let current = dev.names()?.remove(&n).unwrap_or_default();
    if !prst::is_empty(&current) && !force {
        bail!("slot {n} holds {current:?}; refusing to overwrite without --force");
    }
    volume_warning(n);
    let backups = state_dir().join("backups");
    fs::create_dir_all(&backups).with_context(|| format!("creating {}", backups.display()))?;
    let backup = backups.join(format!(
        "{}-{n:02}-{}.prst",
        now("%Y%m%d-%H%M%S"),
        safe(&current)
    ));
    write(&backup, &dev.read_slot(n, Some(&current))?)?;
    println!("backed up slot {n} ({current}) -> {}", backup.display());

    if !yes {
        print!("write {name:?} to slot {n} (now {current:?})? [y/N] ");
        io::stdout().flush()?;
        let mut reply = String::new();
        io::stdin().read_line(&mut reply)?;
        if !matches!(reply.trim().to_lowercase().as_str(), "y" | "yes") {
            bail!("aborted, nothing written");
        }
    }
    let (packets, acks) = dev.write_slot(n, &data)?;
    println!("sent {packets} packets, {acks} acknowledged");
    let readback = dev.read_slot(n, None)?;
    if readback != data {
        let bad = backups.join(format!("readback-{n:02}.prst"));
        write(&bad, &readback)?;
        bail!(
            "read-back does NOT match (saved to {}). Restore with: gp50 push {} {n} --force",
            bad.display(),
            backup.display()
        );
    }
    println!("verified: slot {n} now holds {name:?}, byte-for-byte");
    Ok(())
}

fn cmd_backup(output: Option<PathBuf>) -> Result<()> {
    let out = output.unwrap_or_else(|| format!("gp50_bank_{}.json", now("%Y%m%d-%H%M%S")).into());
    let mut dev = Gp50::open()?;
    let names = dev.names()?;
    eprintln!("reading all 100 slots (about 90 s); the pedal will step through them.");
    let mut presets = Vec::with_capacity(100);
    for n in 0..100u8 {
        let data = dev.read_slot(n, Some(&names[&n]))?;
        presets.push(serde_json::json!({
            "slot": n,
            "name": names[&n],
            "b64": base64::engine::general_purpose::STANDARD.encode(&data),
        }));
        eprint!("\r{}/100", n + 1);
    }
    let taken_at = chrono::Local::now().timestamp_millis();
    let json = serde_json::json!({ "device": "gp50", "takenAt": taken_at, "presets": presets });
    write(&out, json.to_string().as_bytes())?;
    eprintln!();
    println!("100 presets -> {}", out.display());
    Ok(())
}

fn main() -> ExitCode {
    let result = match Cli::parse().cmd {
        Cmd::List { all } => cmd_list(all),
        Cmd::Pull { slot, output } => cmd_pull(slot, output),
        Cmd::Show { source, all } => cmd_show(&source, all),
        Cmd::Edit {
            source,
            output,
            set,
            name,
            vol,
        } => cmd_edit(&source, output, &set, name, vol),
        Cmd::Push {
            file,
            slot,
            force,
            yes,
        } => cmd_push(&file, slot, force, yes),
        Cmd::Backup { output } => cmd_backup(output),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gp50: {e:#}");
            ExitCode::FAILURE
        }
    }
}
