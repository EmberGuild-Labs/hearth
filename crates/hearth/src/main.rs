//! `hearth` — the hub application for the Ember file ecosystem.
//!
//! Hearth is deliberately thin. It resolves a file to a format plugin and
//! then gets out of the way: the container, the hashing, the provenance
//! chain, the encryption and the migration engine all live in `libwick`, and
//! each plugin only describes its own payload. That split is what makes
//! adding a format a day's work rather than a project.

mod identity;
mod json;
mod preview;
mod registry;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use libwick::caps::Capabilities;
use libwick::chunks::{ChunkType, Nested};
use libwick::plugin::{Payload, Plugin, RenderOpts, Source, Starter};
use libwick::schema::Severity;
use libwick::{Flags, Peek, Tag, WickFile};
use registry::Registry;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

const TOOL: &str = concat!("Hearth v", env!("CARGO_PKG_VERSION"));

#[derive(Parser)]
#[command(
    name = "hearth",
    version,
    about = "Convert, view, diff, validate and migrate Ember files",
    long_about = "Hearth is the hub for the Ember file ecosystem: .emt, .emd, .emi, .emc and .emx.\n\
                  Every one of them is a Wick container with a different payload, so one tool \
                  reads all of them.\n\n\
                  Run `hearth formats` to see what this build handles."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Suppress progress and advisory output.
    #[arg(short, long, global = true)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Convert a legacy file into its Ember equivalent, or back again.
    Convert(ConvertArgs),
    /// Start a new, empty Ember file.
    Create(CreateArgs),
    /// Open a file in your editor and put the result back in the container.
    Edit(EditArgs),
    /// Open a file in the Hearth application, in a window.
    Open(FileArg),
    /// Display any Wick file, whatever its format.
    View(ViewArgs),
    /// A one-page preview of a file, as HTML. What Quick Look shows.
    Preview(PreviewArgs),
    /// Compare two files of the same format, semantically.
    Diff(DiffArgs),
    /// Check a file against its embedded schema, rules and provenance.
    Validate(ValidateArgs),
    /// Apply a file's own embedded migration rules.
    Migrate(MigrateArgs),
    /// Walk and verify the provenance signature chain.
    VerifyChain(FileArg),
    /// Header, flags and chunk table, read without decoding the payload.
    Info(InfoArgs),
    /// What this build can read and write.
    Formats(FormatsArgs),
    /// Manage the identity used to sign provenance entries.
    #[command(subcommand)]
    Key(KeyCommand),
    /// Read or attach a file's embedded migration rules.
    #[command(subcommand)]
    Rules(RulesCommand),
    /// Encrypt a whole file behind a passphrase, whatever its format.
    Encrypt(EncryptArgs),
    /// Undo `hearth encrypt`, given the passphrase.
    Decrypt(DecryptArgs),
    /// Move config values into an encrypted key slot (.emc).
    Seal(SealArgs),
    /// Bring sealed values back into the plaintext half (.emc).
    Unseal(UnsealArgs),
    /// Print one config value by dotted path (.emc).
    Get(GetArgs),
    /// Set one config value by dotted path, without an editor (.emc).
    Set(SetArgs),
    /// Remove a config value, or a whole subtree (.emc).
    Unset(UnsetArgs),
    /// Freeze a document's layout so it renders identically everywhere (.emd).
    Pin(PinArgs),
    /// Re-evaluate computed columns (.emx).
    Recompute(FileArg),
    /// Write the preview thumbnail out as a PNG, without decoding the image (.emi).
    Thumbnail(ThumbArgs),
}

#[derive(Subcommand)]
enum RulesCommand {
    /// Print the `MIGR` rules a file carries.
    Show(FileArg),
    /// Embed a rule set from a JSON file.
    ///
    /// This is how a format author ships an upgrade path: write the rules
    /// once and put them into the files you produce, so a reader years later
    /// can bring them forward without knowing anything about the change.
    Set(RulesSetArgs),
}

#[derive(Args)]
struct RulesSetArgs {
    file: PathBuf,
    /// JSON rule set: `{"rules": [{"from": 1, "to": 2, "ops": [...]}]}`.
    rules: PathBuf,
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Create a signing identity.
    Generate,
    /// Show the configured identity's public key.
    Show,
}

#[derive(Args)]
struct FileArg {
    file: PathBuf,
}

#[derive(Args)]
struct InfoArgs {
    file: PathBuf,
    /// Machine-readable output: one JSON document on stdout, nothing else.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ConvertArgs {
    file: PathBuf,
    /// Where to write it. Its extension says which format you want, so
    /// `hearth convert notes.md notes.emd` needs nothing else.
    #[arg(value_name = "OUTPUT")]
    dest: Option<PathBuf>,
    /// Target format. A legacy extension (pdf, csv, json) exports; omit to
    /// import into the Ember equivalent.
    #[arg(long)]
    to: Option<String>,
    /// Which Ember format to import into, when more than one could take the
    /// input: `--as emd` for a .md file, for example.
    #[arg(long = "as", value_name = "EXT")]
    as_format: Option<String>,
    /// Output path, as a flag. The same thing as the second argument.
    #[arg(short, long, conflicts_with = "dest")]
    output: Option<PathBuf>,
    /// A JSON capability declaration to embed (.emc).
    #[arg(long, value_name = "FILE")]
    caps: Option<PathBuf>,
    /// What the bytes on standard input are, when the input is `-`: the
    /// legacy extension they would have had as a file.
    #[arg(long, value_name = "EXT")]
    src: Option<String>,
    /// Replace the output if it already exists.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct CreateArgs {
    /// Path to create. Its extension picks the format: notes.emt, service.emc.
    file: PathBuf,
    /// Format to create, when the path does not already say: `--as emt`.
    #[arg(long = "as", value_name = "EXT")]
    as_format: Option<String>,
    /// Opening heading (.emt, .emd).
    #[arg(long)]
    title: Option<String>,
    /// Columns, in the same syntax a CSV header uses:
    /// `"station, distance (m), speed (m/s) = distance / elapsed"` (.emx).
    #[arg(long, value_name = "LIST")]
    columns: Option<String>,
    /// Canvas size in pixels, `640x480` (.emi).
    #[arg(long, value_name = "WxH")]
    size: Option<String>,
    /// Open the new file in your editor straight away.
    #[arg(short, long)]
    edit: bool,
    /// Editor command for `--edit`. Defaults to $VISUAL, then $EDITOR.
    #[arg(long, value_name = "CMD", requires = "edit")]
    with: Option<String>,
    /// Replace the file if it already exists.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct EditArgs {
    file: PathBuf,
    /// Legacy format to edit in. Defaults to the first one the format both
    /// exports and imports, so the round trip is closed.
    #[arg(long, value_name = "EXT")]
    to: Option<String>,
    /// Editor command. Defaults to $VISUAL, then $EDITOR.
    #[arg(long, value_name = "CMD")]
    with: Option<String>,
    /// Write the editable form here and stop, printing the dialect it is in.
    /// This is half of an edit, for a front end that provides the other half.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["with", "from"])]
    export: Option<PathBuf>,
    /// Take the edited form from here instead of opening an editor. The other
    /// half of `--export`.
    #[arg(long, value_name = "FILE", conflicts_with = "with")]
    from: Option<PathBuf>,
    /// Write the result here instead of over the original.
    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(Args)]
struct PreviewArgs {
    file: PathBuf,
    /// Decode the payload instead of rendering the summary tier.
    #[arg(long)]
    full: bool,
    /// Plain text instead of HTML.
    #[arg(long)]
    text: bool,
    /// Write here instead of to standard output.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Wrap to this width.
    #[arg(short, long)]
    width: Option<usize>,
}

#[derive(Args)]
struct ViewArgs {
    file: PathBuf,
    /// Read only the summary tier. Never touches the payload.
    #[arg(short, long)]
    summary: bool,
    /// Stop after this many sections, rows or blocks.
    #[arg(short, long)]
    limit: Option<usize>,
    /// Wrap to this width.
    #[arg(short, long)]
    width: Option<usize>,
    /// Passphrase-protected chunks to unlock, as `slot:passphrase` or just
    /// `slot` to be prompted.
    #[arg(long, value_name = "SLOT[:PASS]")]
    unlock: Vec<String>,
    /// Read the passphrase from this environment variable instead of asking.
    #[arg(long, value_name = "VAR")]
    passphrase_env: Option<String>,
}

#[derive(Args)]
struct DiffArgs {
    a: PathBuf,
    b: PathBuf,
    /// Machine-readable output: one JSON document on stdout, nothing else.
    #[arg(long)]
    json: bool,
    /// Compare the raw chunk tree instead of asking the plugin for a
    /// semantic answer.
    #[arg(long)]
    structural: bool,
}

#[derive(Args)]
struct ValidateArgs {
    file: PathBuf,
    /// Machine-readable output: one JSON document on stdout, nothing else.
    #[arg(long)]
    json: bool,
    /// A JSON capability policy to check the file's `CAPS` against.
    #[arg(long, value_name = "FILE")]
    policy: Option<PathBuf>,
    #[arg(long, value_name = "SLOT[:PASS]")]
    unlock: Vec<String>,
    /// Read the passphrase from this environment variable instead of asking.
    #[arg(long, value_name = "VAR")]
    passphrase_env: Option<String>,
}

#[derive(Args)]
struct MigrateArgs {
    file: PathBuf,
    /// Target payload schema version. Defaults to the newest the file's own
    /// rules can reach.
    #[arg(long)]
    to: Option<u32>,
    /// Report what would happen and write nothing.
    #[arg(long)]
    dry_run: bool,
    /// Write here instead of in place.
    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(Args)]
struct EncryptArgs {
    file: PathBuf,
    /// Write the encrypted file here instead of over the original.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Key slot number.
    #[arg(long, default_value_t = 1)]
    slot: u8,
    /// What this slot is for. Stored in the clear, so name the purpose, not
    /// the passphrase.
    #[arg(long, default_value = "payload")]
    label: String,
    /// Read the passphrase from this environment variable instead of asking.
    #[arg(long, value_name = "VAR")]
    passphrase_env: Option<String>,
}

#[derive(Args)]
struct DecryptArgs {
    file: PathBuf,
    /// Write the decrypted file here instead of over the original.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Key slot to open. Only needed for a file with more than one.
    #[arg(long)]
    slot: Option<u8>,
    /// Read the passphrase from this environment variable instead of asking.
    #[arg(long, value_name = "VAR")]
    passphrase_env: Option<String>,
}

#[derive(Args)]
struct SealArgs {
    file: PathBuf,
    /// Dotted config paths to seal. A prefix takes everything beneath it.
    #[arg(required_unless_present = "all")]
    paths: Vec<String>,
    /// Seal every value, and the schema and summary tier with them, so the
    /// file gives up nothing but its format.
    #[arg(long, conflicts_with = "paths")]
    all: bool,
    /// Key slot number.
    #[arg(long, default_value_t = 1)]
    slot: u8,
    /// What this slot is for: "prod", "staging".
    #[arg(long, default_value = "secrets")]
    label: String,
    /// Read the passphrase from this environment variable instead of asking.
    #[arg(long, value_name = "VAR")]
    passphrase_env: Option<String>,
}

#[derive(Args)]
struct UnsealArgs {
    file: PathBuf,
    /// Dotted config paths to bring back. A prefix takes everything beneath
    /// it; omit them with `--all` to unseal a whole slot.
    #[arg(required_unless_present = "all")]
    paths: Vec<String>,
    /// Bring back everything this passphrase opens.
    #[arg(long, conflicts_with = "paths")]
    all: bool,
    /// Key slot to open.
    #[arg(long, default_value_t = 1)]
    slot: u8,
    /// Read the passphrase from this environment variable instead of asking.
    #[arg(long, value_name = "VAR")]
    passphrase_env: Option<String>,
}

#[derive(Args)]
struct GetArgs {
    file: PathBuf,
    /// Dotted path: `database.port`.
    path: String,
    /// Print the value as JSON rather than as bare text.
    #[arg(long)]
    json: bool,
    #[arg(long, value_name = "SLOT[:PASS]")]
    unlock: Vec<String>,
    /// Read the passphrase from this environment variable instead of asking.
    #[arg(long, value_name = "VAR")]
    passphrase_env: Option<String>,
}

#[derive(Args)]
struct SetArgs {
    file: PathBuf,
    /// Dotted path: `database.port`.
    path: String,
    /// The new value. Read as JSON when it parses as JSON — `8080` is a
    /// number, `true` a boolean — and as a string when it does not.
    value: String,
    /// Take the value as a string even if it looks like JSON.
    #[arg(long)]
    string: bool,
    /// Add the path even when the file has sealed values that might already
    /// contain it.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct UnsetArgs {
    file: PathBuf,
    /// Dotted path. A prefix removes everything beneath it.
    path: String,
}

#[derive(Args)]
struct PinArgs {
    file: PathBuf,
    /// Remove the pinned layout and return the document to reflowing.
    #[arg(long)]
    undo: bool,
}

#[derive(Args)]
struct FormatsArgs {
    /// Machine-readable output: one JSON document on stdout, nothing else.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ThumbArgs {
    file: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() {
    restore_sigpipe();
    if let Err(e) = run() {
        eprintln!("hearth: {e:#}");
        // The one failure with an obvious next step. Somebody handed an
        // encrypted file has no reason to know which flag opens it, and
        // "no key supplied" on its own does not tell them.
        if let Some(libwick::Error::NeedKey { slot, .. }) = e.chain().find_map(|c| c.downcast_ref())
        {
            eprintln!(
                "       this file is encrypted — `hearth decrypt` opens it for good, \
                 or pass --unlock {slot} to view, validate and get"
            );
        }
        std::process::exit(1);
    }
}

/// Rust sets `SIGPIPE` to ignored, which turns `hearth info big.emx | head`
/// into a panic instead of a quiet exit. Every other command-line tool ends
/// silently when its reader goes away, and a tool that cannot be piped into
/// `head` is a tool nobody will pipe into anything.
#[cfg(unix)]
fn restore_sigpipe() {
    // Safe: setting a signal disposition to the default is exactly what the
    // process would have had if Rust had not changed it before `main`.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let reg = Registry::new();
    let ctx = Ctx {
        quiet: cli.quiet,
        color: std::io::stdout().is_terminal(),
    };
    match cli.command {
        Command::Convert(a) => cmd_convert(&reg, &ctx, a),
        Command::Create(a) => cmd_create(&reg, &ctx, a),
        Command::Edit(a) => cmd_edit(&reg, &ctx, a),
        Command::Open(a) => cmd_open(&reg, &ctx, a),
        Command::View(a) => cmd_view(&reg, &ctx, a),
        Command::Preview(a) => cmd_preview(&reg, a),
        Command::Diff(a) => cmd_diff(&reg, &ctx, a),
        Command::Validate(a) => cmd_validate(&reg, &ctx, a),
        Command::Migrate(a) => cmd_migrate(&reg, &ctx, a),
        Command::VerifyChain(a) => cmd_verify_chain(&ctx, a),
        Command::Info(a) => cmd_info(&reg, &ctx, a),
        Command::Formats(a) => cmd_formats(&reg, a),
        Command::Key(k) => cmd_key(k),
        Command::Rules(r) => cmd_rules(&ctx, r),
        Command::Encrypt(a) => cmd_encrypt(&ctx, a),
        Command::Decrypt(a) => cmd_decrypt(&ctx, a),
        Command::Seal(a) => cmd_seal(&ctx, a),
        Command::Unseal(a) => cmd_unseal(&ctx, a),
        Command::Get(a) => cmd_get(a),
        Command::Set(a) => cmd_set(&ctx, a),
        Command::Unset(a) => cmd_unset(&ctx, a),
        Command::Pin(a) => cmd_pin(&ctx, a),
        Command::Recompute(a) => cmd_recompute(&ctx, a),
        Command::Thumbnail(a) => cmd_thumbnail(a),
    }
}

struct Ctx {
    quiet: bool,
    color: bool,
}

impl Ctx {
    fn note(&self, s: &str) {
        if !self.quiet {
            eprintln!("{}", self.dim(s));
        }
    }

    fn dim(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

fn ext_of(p: &Path) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_string()
}

/// Ask for a passphrase.
///
/// Three sources, in order: an environment variable named by the caller,
/// standard input when it is a pipe, and finally an interactive prompt. The
/// middle case is what makes any of this usable from a script — a tool that
/// can only be driven by a human at a terminal cannot be part of a build.
fn read_passphrase(prompt: &str, env_var: Option<&str>) -> Result<String> {
    if let Some(var) = env_var {
        return std::env::var(var).with_context(|| format!("${var} is not set"));
    }
    if !std::io::stdin().is_terminal() {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let line = line.trim_end_matches(['\n', '\r']).to_string();
        anyhow::ensure!(!line.is_empty(), "no passphrase on standard input");
        return Ok(line);
    }
    Ok(rpassword::prompt_password(prompt)?)
}

/// Open a Wick file and unlock whatever slots were requested.
fn open(path: &Path, unlock: &[String], env_var: Option<&str>) -> Result<WickFile> {
    let f = WickFile::read(path).with_context(|| format!("reading {}", path.display()))?;
    unlock_slots(f, unlock, env_var)
}

/// Open reading only as much of `DATA` as the plugin says a render needs.
///
/// Display is the one place this is safe: nothing downstream of it writes the
/// file, and [`WickFile::to_bytes`] refuses a partial file in case that ever
/// stops being true.
fn open_for_render(
    plugin: &dyn Plugin,
    path: &Path,
    opts: &RenderOpts,
    unlock: &[String],
    env_var: Option<&str>,
) -> Result<WickFile> {
    let f = WickFile::read_partial(path, |taken| plugin.enough(taken, opts))
        .with_context(|| format!("reading {}", path.display()))?;
    unlock_slots(f, unlock, env_var)
}

fn unlock_slots(mut f: WickFile, unlock: &[String], env_var: Option<&str>) -> Result<WickFile> {
    for spec in unlock {
        let (slot, pass) = match spec.split_once(':') {
            Some((s, p)) => (s.parse::<u8>()?, p.to_string()),
            None => {
                let slot: u8 = spec.parse()?;
                let label = f.keys.label(slot);
                (
                    slot,
                    read_passphrase(&format!("passphrase for slot {slot} ({label}): "), env_var)?,
                )
            }
        };
        f.unlock(slot, &pass)
            .with_context(|| format!("unlocking slot {slot}"))?;
    }
    Ok(f)
}

fn plugin_for<'a>(reg: &'a Registry, path: &Path) -> Result<(&'a dyn Plugin, Tag)> {
    let (tag, _) = libwick::sniff_path(path)
        .ok_or_else(|| anyhow::anyhow!("{} is not a Wick file", path.display()))?;
    Ok((reg.by_tag(tag)?, tag))
}

// ---------------------------------------------------------------------------
// convert
// ---------------------------------------------------------------------------

fn cmd_convert(reg: &Registry, ctx: &Ctx, a: ConvertArgs) -> Result<()> {
    // `-` is stdin, so a conversion can sit in the middle of a pipeline
    // without a temporary file to clean up afterwards.
    let from_stdin = a.file.as_os_str() == "-";
    let bytes = if from_stdin {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut buf)
            .context("reading standard input")?;
        buf
    } else {
        std::fs::read(&a.file).with_context(|| format!("reading {}", a.file.display()))?
    };
    let id = identity::load()?;

    // Naming the output names the format you want. `hearth convert notes.md
    // notes.emd` should not also need `--as emd`: the sentence is already
    // unambiguous, and making somebody say it twice is how a tool earns a
    // reputation for being fussy.
    let out_path = a.dest.or(a.output);
    let to_stdout = out_path
        .as_deref()
        .map(|p| p.as_os_str() == "-")
        .unwrap_or(false);
    let out_ext = out_path
        .as_deref()
        .filter(|_| !to_stdout)
        .map(ext_of)
        .filter(|e| !e.is_empty());
    if to_stdout && a.to.is_none() && libwick::sniff(&bytes).is_some() {
        bail!("writing to `-` needs `--to <ext>`: the output has no name to take a format from");
    }

    // Which direction? A Wick file goes out to a legacy format; anything
    // else comes in.
    if let Some((tag, _)) = libwick::sniff(&bytes) {
        let plugin = reg.by_tag(tag)?;
        let target = match (a.to.as_deref(), out_ext.as_deref()) {
            (Some(t), Some(o)) if t.trim_start_matches('.') != o => {
                bail!("--to {t} and an output named .{o} ask for two different formats")
            }
            (Some(t), _) => t.trim_start_matches('.').to_string(),
            (None, Some(o)) => o.to_string(),
            (None, None) => plugin
                .exports()
                .first()
                .ok_or_else(|| anyhow::anyhow!("{} cannot export", plugin.name()))?
                .to_string(),
        };
        if !plugin.exports().contains(&target.as_str()) {
            bail!(
                ".{} exports to {}, not .{target}",
                plugin.ext(),
                plugin.exports().join(", .")
            );
        }
        let file = WickFile::from_bytes(&bytes, true)?;
        let out = plugin.export(&file, &target)?;
        if to_stdout {
            let mut w = std::io::BufWriter::new(std::io::stdout().lock());
            w.write_all(&out)?;
            w.flush()?;
            ctx.note(&format!("exported as .{target} to standard output"));
            return Ok(());
        }
        let out_path = out_path.unwrap_or_else(|| a.file.with_extension(&target));
        guard_output(&out_path, &a.file, a.force)?;
        std::fs::write(&out_path, &out)?;
        println!(
            "{} -> {}  ({} bytes)",
            a.file.display(),
            out_path.display(),
            out.len()
        );
        ctx.note("exported to a legacy format: schema, provenance and summary tier are not carried across");
        return Ok(());
    }

    // Import. The output's extension chooses the Ember format, exactly as
    // `--as` would.
    let ext = match (&a.src, from_stdin) {
        (Some(e), _) => e.trim_start_matches('.').to_ascii_lowercase(),
        (None, true) => bail!(
            "reading from `-` needs `--src <ext>`: bytes on standard input carry no name, \
             and what they are cannot be guessed"
        ),
        (None, false) => ext_of(&a.file),
    };
    let want = match (&a.as_format, &out_ext) {
        (Some(w), Some(o)) if w.trim_start_matches('.') != o => {
            bail!("--as {w} and an output named .{o} ask for two different formats")
        }
        (Some(w), _) => Some(w.trim_start_matches('.').to_string()),
        (None, Some(o)) => {
            // A conversion *into* Ember cannot produce a .md. Saying which
            // flag they wanted is more use than "unknown format".
            if reg.by_ext(o).is_err() {
                bail!(
                    "converting .{ext} produces an Ember file, so the output cannot be \
                     called .{o}. Name it .{} — or, to go the other way, \
                     `hearth convert <a Wick file> --to {o}`",
                    reg.preferred(&ext).map(|(p, _)| p.ext()).unwrap_or("emt")
                );
            }
            Some(o.clone())
        }
        (None, None) => None,
    };
    let (plugin, alternatives) = match &want {
        Some(want) => {
            let p = reg.by_ext(want)?;
            if !p.imports().contains(&ext.as_str()) {
                bail!(".{} does not import .{ext}", p.ext());
            }
            (p, Vec::new())
        }
        None => reg.preferred(&ext)?,
    };
    if !alternatives.is_empty() && want.is_none() {
        ctx.note(&format!(
            ".{ext} could also be imported as .{} — use `--as {}` for that",
            alternatives.join(", ."),
            alternatives[0]
        ));
    }

    let name = if from_stdin {
        format!("standard input (.{ext})")
    } else {
        name_of(&a.file)
    };
    let payload = plugin.import(&Source::new(&bytes, &name, &ext))?;

    let mut file = assemble(plugin, &payload)?;
    if let Some(caps_path) = &a.caps {
        let text = std::fs::read_to_string(caps_path)?;
        let caps: Capabilities = serde_json::from_str(&text)
            .with_context(|| format!("reading {}", caps_path.display()))?;
        file.set_caps(&caps)?;
    }
    file.record(TOOL, &format!("converted from legacy .{ext}"), id.as_ref())?;

    if to_stdout {
        let bytes = file.to_bytes()?;
        let mut w = std::io::BufWriter::new(std::io::stdout().lock());
        w.write_all(&bytes)?;
        w.flush()?;
        ctx.note(&format!("wrote a .{} to standard output", plugin.ext()));
        return Ok(());
    }
    if from_stdin && out_path.is_none() {
        bail!("reading from `-` needs somewhere to write: give an output path, or `-` for stdout");
    }
    let out_path = out_path.unwrap_or_else(|| a.file.with_extension(plugin.ext()));
    guard_output(&out_path, &a.file, a.force)?;
    file.write(&out_path)?;

    let before = bytes.len();
    let after = std::fs::metadata(&out_path)?.len() as usize;
    println!(
        "{} -> {}  ({} -> {}, {:+.0}%)",
        a.file.display(),
        out_path.display(),
        human(before),
        human(after),
        (after as f64 - before as f64) / before.max(1) as f64 * 100.0
    );
    if id.is_none() {
        ctx.note("provenance recorded but unsigned — run `hearth key generate` to sign your edits");
    }
    Ok(())
}

/// Turn what a plugin produced into a container. Every file Hearth writes
/// from scratch — converted or created — is built here, so the two cannot
/// drift apart in some detail nobody thought to compare.
fn assemble(plugin: &dyn Plugin, payload: &Payload) -> Result<WickFile> {
    let mut file = WickFile::new(plugin.tag());
    file.set_data(&payload.data)?;
    if let Some(s) = &payload.schema {
        file.set_schema(s)?;
    }
    if let Some(s) = &payload.summary {
        file.set_summary(s)?;
    }
    if let Some(c) = &payload.caps {
        file.set_caps(c)?;
    }
    if let Some(m) = &payload.migrations {
        file.set_migrations(m)?;
    }
    Ok(file)
}

fn guard_output(out: &Path, input: &Path, force: bool) -> Result<()> {
    if out == input {
        bail!("that would overwrite the input; pass -o to choose a different output");
    }
    if out.exists() && !force {
        bail!(
            "{} already exists; pass --force to replace it",
            out.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

fn cmd_create(reg: &Registry, ctx: &Ctx, a: CreateArgs) -> Result<()> {
    let ext = match &a.as_format {
        Some(e) => e.trim_start_matches('.').to_ascii_lowercase(),
        None => ext_of(&a.file),
    };
    if ext.is_empty() {
        bail!(
            "which format? Give the path an extension — `hearth create notes.emt` — or pass \
             `--as emt`"
        );
    }
    let plugin = reg.by_ext(&ext)?;
    // `hearth create notes --as emt` should still produce notes.emt. A file
    // whose extension does not match its contents is the problem the whole
    // ecosystem is trying to stop having.
    let path = if ext_of(&a.file).is_empty() {
        a.file.with_extension(plugin.ext())
    } else {
        a.file.clone()
    };
    if path.exists() && !a.force {
        bail!(
            "{} already exists; pass --force to replace it",
            path.display()
        );
    }

    let spec = Starter {
        title: a.title.as_deref(),
        columns: a.columns.as_deref(),
        size: a.size.as_deref().map(parse_size).transpose()?,
    };
    // A created file goes in through the ordinary importer, so it is built
    // by the same code as a converted one and cannot differ from it.
    let (src_ext, bytes) = plugin.starter(&spec)?;
    let name = name_of(&path);
    let payload = plugin.import(&Source::new(&bytes, &name, src_ext))?;

    let id = identity::load()?;
    let mut file = assemble(plugin, &payload)?;
    file.record(
        TOOL,
        &format!("created a new .{}", plugin.ext()),
        id.as_ref(),
    )?;
    file.write(&path)?;

    println!(
        "{}  ({}, {})",
        path.display(),
        plugin.name(),
        human(std::fs::metadata(&path)?.len() as usize)
    );
    if id.is_none() {
        ctx.note("provenance recorded but unsigned — run `hearth key generate` to sign your edits");
    }

    if a.edit {
        return cmd_edit(
            reg,
            ctx,
            EditArgs {
                file: path,
                to: None,
                with: a.with,
                export: None,
                from: None,
                output: None,
            },
        );
    }
    ctx.note(&format!(
        "`hearth edit {}` opens it in your editor",
        path.display()
    ));
    Ok(())
}

fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X', '*'])
        .ok_or_else(|| anyhow::anyhow!("--size takes WxH, as in 640x480 (got {s:?})"))?;
    let parse = |v: &str, which| -> Result<u32> {
        v.trim()
            .parse()
            .with_context(|| format!("{which} of --size {s:?} is not a number"))
    };
    Ok((parse(w, "the width")?, parse(h, "the height")?))
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

/// Edit a Wick file by round-tripping its payload through a legacy format.
///
/// Export, edit, re-import. That is the whole mechanism, and it is only
/// honest for formats that import and export the same extension — anything
/// else would quietly return a different file from the one that went out.
/// The container is kept and only its payload chunks are replaced, so the
/// provenance chain, capability declaration and migration rules survive an
/// edit rather than being reborn without a history.
fn cmd_edit(reg: &Registry, ctx: &Ctx, a: EditArgs) -> Result<()> {
    let (plugin, _) = plugin_for(reg, &a.file)?;
    let mut file =
        WickFile::read(&a.file).with_context(|| format!("reading {}", a.file.display()))?;

    let round_trip: Vec<&str> = plugin
        .exports()
        .iter()
        .filter(|e| plugin.imports().contains(e))
        .copied()
        .collect();
    let preferred = plugin.edit_ext(&file)?;
    let target = match &a.to {
        Some(t) => {
            let t = t.trim_start_matches('.').to_ascii_lowercase();
            if !round_trip.iter().any(|e| *e == t) {
                bail!(
                    ".{} can be edited as .{} — .{t} would not come back in",
                    plugin.ext(),
                    round_trip.join(", .")
                );
            }
            if t != preferred {
                ctx.note(&format!(
                    "editing as .{t} rather than .{preferred}: whatever .{t} cannot express \
                     will not survive the round trip"
                ));
            }
            t
        }
        None => preferred.to_string(),
    };

    // Re-importing rebuilds the payload from the edited text, and text that
    // never held the sealed values cannot put them back. Refusing is the
    // only answer that does not lose a secret or duplicate it in plaintext.
    let sealed = file.chunks.iter().filter(|c| c.enc.slot != 0).count();
    if sealed > 0 {
        bail!(
            "{} has {sealed} sealed chunk(s); an edit rewrites the payload and cannot carry \
             them through. Run `hearth unseal {} --all` first, edit, then seal again",
            a.file.display(),
            a.file.display()
        );
    }

    // An edit writes this build's payload schema. Doing that to a file at a
    // different version would migrate it silently, as a side effect of
    // opening an editor.
    if let Some(schema) = file.schema()? {
        let mine = plugin.schema_version();
        if schema.version != mine {
            bail!(
                "{} is at payload schema v{}, this build writes v{mine}. Run `hearth migrate` \
                 first — an edit must not change the schema behind your back",
                a.file.display(),
                schema.version
            );
        }
    }

    let out_path = a.output.clone().unwrap_or_else(|| a.file.clone());
    let before = plugin.export(&file, &target)?;

    // `--export` hands the editable form to somebody else — the Hearth
    // application, a script, another editor entirely — and stops. The
    // dialect goes to stdout so the caller knows what it was given without
    // having to know this table itself.
    if let Some(dest) = &a.export {
        std::fs::write(dest, &before).with_context(|| format!("writing {}", dest.display()))?;
        println!("{target}");
        ctx.note(&format!(
            "wrote {} as .{target} to {}",
            name_of(&a.file),
            dest.display()
        ));
        return Ok(());
    }

    // Where the edited bytes come back from: a file somebody else already
    // edited, or an editor this command launches.
    let (after, scratch) = match &a.from {
        Some(p) => (
            std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
            None,
        ),
        None => {
            let dir = std::env::temp_dir().join(format!("hearth-edit-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            let stem = a
                .file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("file")
                .to_string();
            let scratch = dir.join(format!("{stem}.{target}"));
            std::fs::write(&scratch, &before)?;

            let editor = Editor::resolve(a.with.as_deref(), &target)?;
            ctx.note(&format!(
                "editing {} as .{target} in {}",
                name_of(&a.file),
                editor.describe()
            ));
            let launched = editor.run(&scratch, ctx);
            if launched.is_err() {
                eprintln!("your work is at {}", scratch.display());
            }
            launched?;
            (std::fs::read(&scratch)?, Some(scratch))
        }
    };

    if after == before {
        if let Some(s) = &scratch {
            let _ = std::fs::remove_dir_all(s.parent().unwrap_or(s));
        }
        println!("no changes; {} is untouched", a.file.display());
        return Ok(());
    }

    // From here on the edited text is the only copy of the user's work, so
    // the scratch file stays on disk until it has been safely stored.
    let payload = plugin
        .import(&Source::new(&after, &name_of(&a.file), &target))
        .inspect_err(|_| {
            if let Some(s) = &scratch {
                eprintln!("your edit is still at {}", s.display());
            }
        })?;

    file.set_data(&payload.data)?;
    match &payload.schema {
        Some(s) => file.set_schema(s)?,
        None => {
            file.chunks.remove(ChunkType::SCHM);
        }
    }
    match &payload.summary {
        Some(s) => file.set_summary(s)?,
        // A summary describing the previous payload is worse than none: it
        // is the tier every cheap reader trusts, and it would now be a
        // description of a document that no longer exists.
        None => {
            file.chunks.remove(ChunkType::SUMM);
        }
    }
    // CAPS and MIGR belong to the file, not to this edit. A plugin that
    // infers them from a legacy source only does so for a file that has
    // none yet.
    if let (Some(c), None) = (&payload.caps, file.caps()?) {
        file.set_caps(c)?;
    }
    if let (Some(m), None) = (&payload.migrations, file.migrations()?) {
        file.set_migrations(m)?;
    }

    let original = WickFile::read(&a.file)?;
    let changes = plugin.diff(&original, &file, &file.keys)?;
    file.record(
        TOOL,
        &format!("edited as .{target}: {} change(s)", changes.len()),
        identity::load()?.as_ref(),
    )?;
    file.write(&out_path)?;
    if let Some(s) = &scratch {
        let _ = std::fs::remove_dir_all(s.parent().unwrap_or(s));
    }

    println!("{}  ({} change(s))", out_path.display(), changes.len());
    const SHOWN: usize = 12;
    for c in changes.iter().take(SHOWN) {
        let colour = match c.kind {
            libwick::ChangeKind::Added => "32",
            libwick::ChangeKind::Removed => "31",
            libwick::ChangeKind::Modified => "33",
            libwick::ChangeKind::Moved => "36",
        };
        println!(
            "{} {}  {}",
            ctx.paint(colour, &c.kind.sigil().to_string()),
            c.path,
            ctx.dim(&c.note)
        );
    }
    if changes.len() > SHOWN {
        println!(
            "{}",
            ctx.dim(&format!("… and {} more", changes.len() - SHOWN))
        );
    }
    Ok(())
}

/// Hand a file to the Hearth application.
///
/// The window is the other half of `hearth edit`: same container, same
/// round trip, same provenance entry, but with the editing done in a window
/// instead of an editor the terminal is waiting on. The file is checked
/// first, because "that is not a Wick file" is more use here than an
/// application opening and showing an error of its own.
fn cmd_open(reg: &Registry, ctx: &Ctx, a: FileArg) -> Result<()> {
    let (plugin, _) = plugin_for(reg, &a.file)?;
    if !cfg!(target_os = "macos") {
        bail!(
            "the Hearth application is macOS-only for now. `hearth edit {}` opens it in \
             $EDITOR instead",
            a.file.display()
        );
    }
    let status = std::process::Command::new("open")
        .arg("-a")
        .arg("Hearth")
        .arg(&a.file)
        .status()
        .context("could not run `open`")?;
    if !status.success() {
        bail!(
            "could not open Hearth. Install it with macos/build-app.sh, or use \
             `hearth edit {}`",
            a.file.display()
        );
    }
    ctx.note(&format!(
        "opened {} ({}) in Hearth",
        name_of(&a.file),
        plugin.name()
    ));
    Ok(())
}

/// The command that opens a file, and where it came from.
struct Editor {
    program: String,
    args: Vec<String>,
    source: &'static str,
    /// True when the command hands the file to a windowed application and
    /// returns, rather than occupying the terminal until the editing is
    /// finished.
    windowed: bool,
}

impl Editor {
    /// `--with` first, then `$VISUAL`, then `$EDITOR`, then the platform's
    /// own opener.
    ///
    /// `$EDITOR` is deliberately skipped for binary payloads: `vi` opening a
    /// PNG is not an editing session, it is a corrupted file waiting to
    /// happen.
    fn resolve(with: Option<&str>, target: &str) -> Result<Editor> {
        let textual = matches!(
            target,
            "txt"
                | "text"
                | "md"
                | "markdown"
                | "rst"
                | "log"
                | "csv"
                | "tsv"
                | "json"
                | "yaml"
                | "yml"
                | "toml"
        );
        if let Some(cmd) = with {
            return Ok(Editor::parse(cmd, "--with"));
        }
        if textual {
            for var in ["VISUAL", "EDITOR"] {
                if let Ok(cmd) = std::env::var(var) {
                    if !cmd.trim().is_empty() {
                        return Ok(Editor::parse(
                            &cmd,
                            if var == "VISUAL" {
                                "$VISUAL"
                            } else {
                                "$EDITOR"
                            },
                        ));
                    }
                }
            }
        }
        if cfg!(target_os = "macos") {
            return Ok(Editor {
                program: "open".into(),
                args: if textual {
                    vec!["-t".to_string()]
                } else {
                    Vec::new()
                },
                source: "the default macOS application",
                windowed: true,
            });
        }
        bail!("no editor: set $EDITOR, or pass --with 'code -w'")
    }

    /// Split on whitespace, so `--with 'code -w'` works. Anything that needs
    /// a shell can be wrapped in one by the caller.
    ///
    /// A command given by name is trusted to block until the editing is
    /// done — that is what `-w` in `code -w` is for, and second-guessing
    /// somebody who has just told you how to run their editor is worse than
    /// believing them.
    fn parse(cmd: &str, source: &'static str) -> Editor {
        let mut parts = cmd.split_whitespace().map(|s| s.to_string());
        Editor {
            program: parts.next().unwrap_or_else(|| "vi".into()),
            args: parts.collect(),
            source,
            windowed: false,
        }
    }

    fn describe(&self) -> String {
        format!("{} ({})", self.program, self.source)
    }

    /// Run the editor and return once the editing is finished.
    ///
    /// "Finished" is the hard part. A terminal editor occupies the terminal
    /// and finishing means exiting, so waiting on the process is exactly
    /// right. A windowed application does not work like that: `open -W`
    /// waits for the *application* to quit, which is a different event from
    /// saving a file and often never happens at all — TextEdit that was
    /// already running has nothing to quit. Waiting on the wrong event is
    /// how an edit gets read back before it was saved, and silently lost.
    ///
    /// So for a windowed editor, wait for the person instead.
    fn run(&self, path: &Path, ctx: &Ctx) -> Result<()> {
        if self.windowed && std::io::stdin().is_terminal() {
            std::process::Command::new(&self.program)
                .args(&self.args)
                .arg(path)
                .status()
                .with_context(|| format!("could not run {}", self.program))?;
            eprint!(
                "{}",
                ctx.dim("save in the editor, then press Enter here (Ctrl-C to abandon the edit): ")
            );
            std::io::stderr().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            return Ok(());
        }

        // A windowed editor with nobody at a terminal — a script — gets the
        // old behaviour, because there is nobody to ask and the application
        // quitting is the only signal there is.
        let mut args = self.args.clone();
        if self.windowed {
            args.insert(0, "-W".to_string());
        }
        let status = std::process::Command::new(&self.program)
            .args(&args)
            .arg(path)
            .status()
            .with_context(|| format!("could not run {}", self.program))?;
        if !status.success() {
            bail!("{} exited with {status}; nothing was written", self.program);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// view / info / formats
// ---------------------------------------------------------------------------

fn cmd_view(reg: &Registry, ctx: &Ctx, a: ViewArgs) -> Result<()> {
    let (plugin, _) = plugin_for(reg, &a.file)?;

    // The summary path is meant to be cheap, so it must not be a full read
    // dressed up as one: check the tier exists from the header alone first.
    if a.summary {
        let peek = Peek::open(&a.file)?;
        if !peek.has(ChunkType::SUMM) {
            bail!(
                "{} has no summary tier; run without --summary",
                a.file.display()
            );
        }
    }

    let opts = RenderOpts {
        summary: a.summary,
        width: a.width.unwrap_or_else(term_width),
        limit: a.limit,
        color: ctx.color,
    };
    // Viewing is where a partial read belongs: a view of twenty rows should
    // not cost a million, and nothing downstream of here writes the file.
    let file = open_for_render(
        plugin,
        &a.file,
        &opts,
        &a.unlock,
        a.passphrase_env.as_deref(),
    )?;
    // A tier that is present but sealed is not a missing tier. Saying "no
    // summary" here would send somebody looking for a file that is right
    // there, when what they need is a passphrase.
    if a.summary {
        if let Some(slot) = file.sealed_slot(ChunkType::SUMM) {
            bail!(
                "the summary tier of {} is sealed to slot {slot} ({}); pass --unlock {slot}",
                a.file.display(),
                file.keys.label(slot)
            );
        }
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    plugin.render(&file, &opts, &mut out)?;
    out.flush()?;
    Ok(())
}

/// One page describing a file, for something that is not a terminal.
///
/// Quick Look is the caller this exists for: it hands the extension a path
/// and expects a rendered pane back, for a file the user has not opened and
/// may not open. That is what the summary tier is for, so the default reads
/// it and leaves `DATA` alone.
fn cmd_preview(reg: &Registry, a: PreviewArgs) -> Result<()> {
    let (plugin, _) = plugin_for(reg, &a.file)?;
    let opts = preview::Opts {
        full: a.full,
        width: a.width.unwrap_or(72),
    };
    let page = if a.text {
        preview::text(plugin, &a.file, &opts)?
    } else {
        preview::html(plugin, &a.file, &opts)?
    };
    match &a.output {
        Some(p) => std::fs::write(p, page.as_bytes())
            .with_context(|| format!("writing {}", p.display()))?,
        None => {
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            out.write_all(page.as_bytes())?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Where `DATA`'s sub-chunks lie, so a caller can read one row group or one
/// tile instead of a payload.
///
/// `addressable: false` is a fact about the layout, not a failure: a payload
/// of many small children is stored as one compressed stream because that is
/// how it compresses, and it is small enough that seeking into it would save
/// nothing. `why` says which case it is, because "compressed" wants a
/// different response from a caller than "sealed".
fn payload_index(peek: &Peek, path: &Path) -> Result<serde_json::Value> {
    Ok(match peek.children(path, ChunkType::DATA)? {
        None => serde_json::Value::Null,
        Some(Nested::Addressable(kids)) => serde_json::json!({
            "addressable": true,
            "children": kids.iter().map(|k| serde_json::json!({
                "type": k.ty.to_string(),
                "offset": k.at,
                "length": k.stored,
            })).collect::<Vec<_>>(),
        }),
        Some(Nested::Compressed) => serde_json::json!({
            "addressable": false,
            "why": "compressed",
        }),
        Some(Nested::Sealed(slot)) => serde_json::json!({
            "addressable": false,
            "why": "sealed",
            "slot": slot,
        }),
    })
}

/// The provenance chain, read on its own.
///
/// `hearth info` used to reach this through a whole-file read, which made a
/// command documented as "no payload read" the most expensive way to look at
/// a large file — 67 MB of memory on a 20 MB table, to print four lines about
/// a chain of 271 bytes. The content hash is still checked, streamed rather
/// than buffered, so nothing about the report got weaker.
fn info_chain(peek: &Peek, path: &Path) -> Result<libwick::Chain> {
    peek.verify(path)?;
    match peek.read_chunk(path, ChunkType::PROV, &libwick::KeyRing::empty())? {
        // A sealed chain reads as no chain, which is all a reader without the
        // passphrase can do with it. `hearth verify-chain` says which it is.
        Some(c) if !c.is_locked() => Ok(libwick::Chain::decode(&c)?),
        _ => Ok(libwick::Chain::new()),
    }
}

fn cmd_info(reg: &Registry, ctx: &Ctx, a: InfoArgs) -> Result<()> {
    // Everything here comes from the header and the chunk offsets. No
    // payload is decoded, which is the point: `hearth info` on a gigabyte
    // file costs the same as on a small one.
    let peek = Peek::open(&a.file).with_context(|| format!("reading {}", a.file.display()))?;
    let plugin = reg.by_tag(peek.tag()).ok();

    if a.json {
        let chain = if peek.header.flags.has(Flags::PROVENANCE) {
            let chain = info_chain(&peek, &a.file)?;
            let report = chain.verify();
            serde_json::json!({
                "entries": report.entries,
                "signed": report.signed,
                "unsigned": report.unsigned,
                "intact": report.is_intact(),
                "latest": chain.entries().last().map(|e| serde_json::json!({
                    "timestamp": e.timestamp,
                    "action": e.action,
                    "tool": e.tool,
                    "signed": e.is_signed(),
                    "key": e.key,
                })),
            })
        } else {
            serde_json::Value::Null
        };
        return json::emit(&serde_json::json!({
            "file": a.file.display().to_string(),
            "tag": peek.tag().to_string(),
            "ext": plugin.map(|p| p.ext()),
            "format": plugin.map(|p| p.name()),
            "spec_version": peek.version().to_string(),
            "outdated": peek.is_outdated(),
            "size": peek.file_len,
            "flags": peek.header.flags.names(),
            "content_hash": libwick::hex::encode(&peek.header.content_hash),
            "chunks": peek.chunks.iter().map(|c| serde_json::json!({
                "type": c.ty.to_string(),
                "offset": c.at,
                "length": c.stored,
            })).collect::<Vec<_>>(),
            "payload": payload_index(&peek, &a.file)?,
            "provenance": chain,
        }));
    }

    println!("file:      {}", a.file.display());
    println!(
        "format:    {} (.{}{})",
        peek.tag(),
        plugin.map(|p| p.ext()).unwrap_or("?"),
        plugin
            .map(|p| format!(", {}", p.name()))
            .unwrap_or_default()
    );
    println!(
        "spec:      Wick v{}{}",
        peek.version(),
        if peek.is_outdated() {
            format!(
                " (this build writes v{}; `hearth migrate` can update it)",
                libwick::SPEC_VERSION
            )
        } else {
            String::new()
        }
    );
    let names = peek.header.flags.names();
    println!(
        "flags:     0x{:08x}{}",
        peek.header.flags.0,
        if names.is_empty() {
            String::new()
        } else {
            format!("  {}", names.join(", "))
        }
    );
    println!("size:      {}", human(peek.file_len as usize));
    println!(
        "table:     {} chunks, {} at offset {}",
        peek.chunks.len(),
        human(peek.header.table_len as usize),
        peek.header.table_offset
    );
    println!(
        "hash:      {}",
        libwick::hex::encode(&peek.header.content_hash)
    );
    println!("\nchunks:");
    for c in &peek.chunks {
        let (ty, at, len) = (&c.ty, &c.at, &c.stored);
        let purpose = match *ty {
            ChunkType::DATA => "full-fidelity payload",
            ChunkType::SUMM => "summary / preview tier",
            ChunkType::SCHM => "embedded schema",
            ChunkType::PROV => "provenance chain",
            ChunkType::CAPS => "capability declaration",
            ChunkType::MIGR => "migration rules",
            ChunkType::KEYS => "encryption key slots",
            _ => "format-specific",
        };
        println!(
            "  {ty}  {:>10}  at {:<10} {}",
            human(*len as usize),
            at,
            ctx.dim(purpose)
        );
    }

    // How the payload is laid out one level down. Still one seek per
    // sub-chunk and nothing decoded, and it is what says whether a reader can
    // fetch one row group or one tile without the rest.
    match peek.children(&a.file, ChunkType::DATA)? {
        Some(Nested::Addressable(kids)) if !kids.is_empty() => {
            println!(
                "\npayload:   {} sub-chunks, {}",
                kids.len(),
                ctx.dim("each readable on its own")
            );
            let mut tally: Vec<(ChunkType, usize, u64)> = Vec::new();
            for k in &kids {
                match tally.iter_mut().find(|(t, _, _)| *t == k.ty) {
                    Some(e) => {
                        e.1 += 1;
                        e.2 += k.stored;
                    }
                    None => tally.push((k.ty, 1, k.stored)),
                }
            }
            for (ty, n, bytes) in tally {
                println!("  {ty}  {:>10}  ×{n}", human(bytes as usize));
            }
        }
        Some(Nested::Compressed) => println!(
            "\npayload:   one compressed stream, {}",
            ctx.dim("too small in its parts to be worth reading separately")
        ),
        Some(Nested::Sealed(slot)) => println!("\npayload:   sealed to key slot {slot}"),
        _ => {}
    }

    // The chain is small, so summarising it here is still cheap.
    if peek.header.flags.has(Flags::PROVENANCE) {
        let chain = info_chain(&peek, &a.file)?;
        let report = chain.verify();
        println!(
            "\nprovenance: {} entries, {} signed{}",
            report.entries,
            report.signed,
            if report.is_intact() {
                String::new()
            } else {
                format!(" — {}", ctx.paint("31", "CHAIN BROKEN"))
            }
        );
        if let Some(last) = chain.entries().last() {
            println!(
                "  latest: {} — {} ({})",
                last.timestamp, last.action, last.tool
            );
        }
    }
    Ok(())
}

fn cmd_formats(reg: &Registry, a: FormatsArgs) -> Result<()> {
    if a.json {
        // What a tool needs in order to decide what this build can do with a
        // file, without running it once per format to find out.
        return json::emit(&serde_json::json!({
            "spec_version": libwick::SPEC_VERSION.to_string(),
            "tool": TOOL,
            "formats": reg.iter().map(|p| serde_json::json!({
                "ext": p.ext(),
                "tag": p.tag().to_string(),
                "name": p.name(),
                "about": p.about(),
                "schema_version": p.schema_version(),
                "imports": p.imports(),
                "exports": p.exports(),
            })).collect::<Vec<_>>(),
        }));
    }
    println!("Ember formats, all of them Wick containers with different payloads:\n");
    for p in reg.iter() {
        println!("  .{:<4} {}  {}", p.ext(), p.tag(), p.about());
        println!(
            "        imports .{}   exports .{}",
            p.imports().join(", ."),
            p.exports().join(", .")
        );
    }
    println!("\n  .embr    AR  content-addressed, deduplicated archive (a separate project)");
    println!("\nSpine: Wick v{}", libwick::SPEC_VERSION);
    Ok(())
}

fn term_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(80)
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

fn cmd_diff(reg: &Registry, ctx: &Ctx, a: DiffArgs) -> Result<()> {
    let (pa, ta) = plugin_for(reg, &a.a)?;
    let (_, tb) = plugin_for(reg, &a.b)?;
    if ta != tb {
        bail!("cannot compare a {ta} file with a {tb} file");
    }
    let (fa, fb) = (WickFile::read(&a.a)?, WickFile::read(&a.b)?);

    let changes = if a.structural {
        libwick::diff::structural_all(&fa.chunks, &fb.chunks, &fa.keys)
    } else {
        pa.diff(&fa, &fb, &fa.keys)?
    };

    if a.json {
        // Emitted before the exit status is decided, so a caller reading
        // stdout gets the same document whether or not the files differ.
        json::emit(&serde_json::json!({
            "a": a.a.display().to_string(),
            "b": a.b.display().to_string(),
            "identical": changes.is_empty(),
            "changes": changes.iter().map(json::change).collect::<Vec<_>>(),
            "counts": {
                "added": changes.iter().filter(|c| c.kind == libwick::ChangeKind::Added).count(),
                "removed": changes.iter().filter(|c| c.kind == libwick::ChangeKind::Removed).count(),
                "modified": changes.iter().filter(|c| c.kind == libwick::ChangeKind::Modified).count(),
                "moved": changes.iter().filter(|c| c.kind == libwick::ChangeKind::Moved).count(),
            },
        }))?;
        if changes.is_empty() {
            return Ok(());
        }
        std::process::exit(1);
    }

    if changes.is_empty() {
        println!("{}", ctx.dim("no differences"));
        return Ok(());
    }
    let mut out = std::io::BufWriter::new(std::io::stdout().lock());
    for c in &changes {
        let colour = match c.kind {
            libwick::ChangeKind::Added => "32",
            libwick::ChangeKind::Removed => "31",
            libwick::ChangeKind::Modified => "33",
            libwick::ChangeKind::Moved => "36",
        };
        writeln!(
            out,
            "{} {}  {}",
            ctx.paint(colour, &c.kind.sigil().to_string()),
            c.path,
            ctx.dim(&c.note)
        )?;
    }
    out.flush()?;

    let count = |k: libwick::ChangeKind| changes.iter().filter(|c| c.kind == k).count();
    eprintln!(
        "{}",
        ctx.dim(&format!(
            "{} changes: {} added, {} removed, {} modified, {} moved",
            changes.len(),
            count(libwick::ChangeKind::Added),
            count(libwick::ChangeKind::Removed),
            count(libwick::ChangeKind::Modified),
            count(libwick::ChangeKind::Moved),
        ))
    );
    // Conventional exit status for a diff tool: 1 means "there were
    // differences", so this composes in scripts.
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

fn cmd_validate(reg: &Registry, ctx: &Ctx, a: ValidateArgs) -> Result<()> {
    let (plugin, _) = plugin_for(reg, &a.file)?;
    let file = open(&a.file, &a.unlock, a.passphrase_env.as_deref())?;
    let mut issues = Vec::new();

    // The content hash was already checked by the read; saying so is worth
    // a line, because "it opened" is itself a result. Not in JSON mode,
    // where stdout belongs entirely to the document.
    if !a.json {
        println!("{}", ctx.dim("content hash verified"));
    }

    if let Some(schema) = file.schema()? {
        let expected = plugin.name();
        if schema.kind != expected && !(expected == "table" && schema.kind == "table") {
            issues.push(libwick::Issue::warning(
                "SCHM",
                format!(
                    "schema describes '{}', this is a {expected} file",
                    schema.kind
                ),
            ));
        }
        if schema.version > plugin.schema_version() {
            issues.push(libwick::Issue::warning(
                "SCHM",
                format!(
                    "payload schema v{} is newer than this build understands (v{})",
                    schema.version,
                    plugin.schema_version()
                ),
            ));
        }
    }
    issues.extend(plugin.validate(&file)?);

    if let Some(caps) = file.caps()? {
        if let Some(policy_path) = &a.policy {
            let policy: Capabilities = serde_json::from_str(&std::fs::read_to_string(policy_path)?)
                .with_context(|| format!("reading {}", policy_path.display()))?;
            let violations = caps.exceeds(&policy);
            if violations.is_empty() && !a.json {
                println!("{}", ctx.dim("capabilities are within policy"));
            }
            issues.extend(violations);
        }
    } else if a.policy.is_some() {
        issues.push(libwick::Issue::note(
            "",
            "a policy was supplied but the file declares no capabilities",
        ));
    }

    let report = file.chain()?.verify();
    match &report.broken_at {
        Some(e) => issues.push(libwick::Issue::error("PROV", e.to_string())),
        None if report.entries > 0 && !a.json => println!(
            "{}",
            ctx.dim(&format!(
                "provenance chain intact: {} entries, {} signed",
                report.entries, report.signed
            ))
        ),
        None => {}
    }

    let errors = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .count();
    if a.json {
        json::emit(&serde_json::json!({
            "file": a.file.display().to_string(),
            "ok": errors == 0,
            "content_hash": "verified",
            "provenance": {
                "entries": report.entries,
                "signed": report.signed,
                "intact": report.broken_at.is_none(),
                "broken_at": report.broken_at.as_ref().map(|e| e.to_string()),
            },
            "errors": errors,
            "issues": issues.iter().map(json::issue).collect::<Vec<_>>(),
        }))?;
        if errors > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }

    for i in &issues {
        let (colour, label) = match i.severity {
            Severity::Error => ("31", "error"),
            Severity::Warning => ("33", "warning"),
            Severity::Note => ("36", "note"),
        };
        if i.path.is_empty() {
            println!("{} {}", ctx.paint(colour, label), i.message);
        } else {
            println!("{} {}: {}", ctx.paint(colour, label), i.path, i.message);
        }
    }

    if errors > 0 {
        bail!("{errors} error(s) — the file does not satisfy its own rules");
    }
    println!("{}", ctx.paint("32", "ok"));
    Ok(())
}

// ---------------------------------------------------------------------------
// migrate
// ---------------------------------------------------------------------------

fn cmd_migrate(reg: &Registry, ctx: &Ctx, a: MigrateArgs) -> Result<()> {
    let (plugin, _) = plugin_for(reg, &a.file)?;
    let mut file = WickFile::read(&a.file)?;

    let Some(rules) = file.migrations()? else {
        bail!(
            "{} carries no MIGR rules, so it has no upgrade path of its own. \
             A file written before its format changed can only be migrated by a tool that \
             knows both versions",
            a.file.display()
        );
    };
    let from = file.schema()?.map(|s| s.version).unwrap_or(1);
    let to = a.to.unwrap_or_else(|| rules.latest_from(from));
    if from == to {
        println!("already at payload schema v{from}; nothing to do");
        return Ok(());
    }

    let mut data = file.data()?;
    let report = libwick::migrate::apply(&rules, &mut data, from, to, &mut |op, d| {
        plugin.migrate_op(op, d)
    })?;

    println!("payload schema v{from} -> v{to}");
    for step in &report.steps {
        println!("  {}", ctx.dim(step));
    }
    if a.dry_run {
        println!("{}", ctx.dim("dry run: nothing written"));
        return Ok(());
    }

    file.set_data(&data)?;
    if let Some(mut schema) = file.schema()? {
        schema.version = to;
        file.set_schema(&schema)?;
    }
    // The summary was derived from the old payload, so it is now a
    // description of a document that no longer exists. Rebuilding it is not
    // optional.
    if let Some(summ) = plugin.summarize(&data)? {
        file.set_summary(&summ)?;
    }
    file.record(
        TOOL,
        &format!("migrated payload schema v{from} to v{to}"),
        identity::load()?.as_ref(),
    )?;

    let out = a.output.unwrap_or_else(|| a.file.clone());
    file.write(&out)?;
    println!("wrote {}", out.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// verify-chain
// ---------------------------------------------------------------------------

fn cmd_verify_chain(ctx: &Ctx, a: FileArg) -> Result<()> {
    let file = WickFile::read(&a.file)?;
    let chain = file.chain()?;
    if chain.is_empty() {
        println!("{} has no provenance chain", a.file.display());
        return Ok(());
    }

    for (i, e) in chain.entries().iter().enumerate() {
        let mark = if e.is_signed() {
            ctx.paint("32", "signed  ")
        } else {
            ctx.dim("unsigned")
        };
        println!("{i:>3}  {mark}  {}  {}", e.timestamp, e.action);
        println!("      {}", ctx.dim(&format!("by {}", e.tool)));
        if let Some(k) = &e.key {
            println!(
                "      {}",
                ctx.dim(&format!("key {}…", &k[..16.min(k.len())]))
            );
        }
    }

    let report = chain.verify();
    println!();
    match report.broken_at {
        Some(e) => bail!("{e}"),
        None => {
            println!(
                "{} {} entries, {} signed by {} key(s), {} unsigned",
                ctx.paint("32", "chain intact:"),
                report.entries,
                report.signed,
                report.signers.len(),
                report.unsigned
            );
            if report.unsigned > 0 {
                ctx.note(
                    "unsigned entries cannot be attributed. They are still tamper-evident: \
                     each links to the hash of the one before it.",
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// key
// ---------------------------------------------------------------------------

fn cmd_key(k: KeyCommand) -> Result<()> {
    match k {
        KeyCommand::Generate => {
            let (id, path) = identity::generate()?;
            println!("wrote {}", path.display());
            println!("public key: {}", id.public_hex());
            println!("\nProvenance entries Hearth writes from now on will be signed with it.");
            println!("Share the public key with anyone who needs to verify your files.");
        }
        KeyCommand::Show => match identity::load()? {
            Some(id) => println!("{}", id.public_hex()),
            None => {
                println!("no identity configured");
                println!("run `hearth key generate`, or set $HEARTH_KEY to a hex secret key");
            }
        },
    }
    Ok(())
}

fn cmd_rules(ctx: &Ctx, r: RulesCommand) -> Result<()> {
    match r {
        RulesCommand::Show(a) => {
            let file = WickFile::read(&a.file)?;
            let at = file.schema()?.map(|s| s.version).unwrap_or(1);
            match file.migrations()? {
                None => println!("{} carries no migration rules", a.file.display()),
                Some(rules) => {
                    println!(
                        "payload schema is at v{at}, rules reach v{}",
                        rules.latest_from(at)
                    );
                    for rule in &rules.rules {
                        println!(
                            "\nv{} -> v{}{}",
                            rule.from,
                            rule.to,
                            rule.note
                                .as_ref()
                                .map(|n| format!("  ({n})"))
                                .unwrap_or_default()
                        );
                        for op in &rule.ops {
                            let args: Vec<String> =
                                op.args.iter().map(|(k, v)| format!("{k}={v}")).collect();
                            println!("  {} {}", op.op, ctx.dim(&args.join(" ")));
                        }
                    }
                }
            }
        }
        RulesCommand::Set(a) => {
            let text = std::fs::read_to_string(&a.rules)
                .with_context(|| format!("reading {}", a.rules.display()))?;
            let rules: libwick::RuleSet = serde_json::from_str(&text)
                .with_context(|| format!("{} is not a valid rule set", a.rules.display()))?;

            // Reject a rule set that cannot be planned before it is stored:
            // a file carrying rules that do not connect is worse than one
            // carrying none, because it promises an upgrade path it lacks.
            let mut file = WickFile::read(&a.file)?;
            let from = file.schema()?.map(|s| s.version).unwrap_or(1);
            let target = rules.latest_from(from);
            if target != from {
                rules.plan(from, target).map_err(anyhow::Error::new)?;
            }

            file.set_migrations(&rules)?;
            file.record(
                TOOL,
                &format!("embedded {} migration rule(s)", rules.rules.len()),
                identity::load()?.as_ref(),
            )?;
            file.write(&a.file)?;
            println!(
                "embedded {} rule(s); the file can now migrate itself from v{from} to v{target}",
                rules.rules.len()
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// format-specific commands
// ---------------------------------------------------------------------------

/// Ask for a passphrase the way a thing being locked deserves: twice when a
/// human is typing, once when a script supplies it, and never shorter than
/// the only thing standing between the file and whoever has it.
fn new_passphrase(what: &str, env_var: Option<&str>) -> Result<String> {
    let pass = read_passphrase(&format!("passphrase for {what}: "), env_var)?;
    if env_var.is_none() && std::io::stdin().is_terminal() {
        let again = read_passphrase("repeat: ", None)?;
        if pass != again {
            bail!("passphrases do not match");
        }
    }
    if pass.chars().count() < 8 {
        bail!(
            "use at least 8 characters — this is the only thing standing between \
             the file and anyone who has it"
        );
    }
    Ok(pass)
}

/// Encrypt every chunk of content in a file, whatever format it is.
///
/// `seal` is the `.emc` operation: it takes named values out of a config and
/// locks those, leaving the rest of the file readable, which is the point of
/// split trust. This is the other thing people want, and until now there was
/// no way to ask for it: lock the *whole* payload of any of the five formats
/// so the file can be handed to someone over a channel you do not trust.
///
/// What it cannot do is hide that the file exists or what kind of file it is.
/// The header and chunk table stay readable by design — a Wick file is
/// identifiable as one at a glance, forever — so this says so plainly rather
/// than letting the extension imply more privacy than it has.
fn cmd_encrypt(ctx: &Ctx, a: EncryptArgs) -> Result<()> {
    let mut file =
        WickFile::read(&a.file).with_context(|| format!("reading {}", a.file.display()))?;

    if let Some(slot) = file.locked_slots().first() {
        bail!(
            "{} already has chunks encrypted to slot {slot} ({}); \
             `hearth decrypt` it first, or use `hearth seal` to add a second slot",
            a.file.display(),
            file.keys.label(*slot)
        );
    }

    let pass = new_passphrase(
        &format!("slot {} ({})", a.slot, a.label),
        a.passphrase_env.as_deref(),
    )?;

    file.add_key_slot(a.slot, &a.label, &pass)?;
    let sealed = file.seal_payload(a.slot);
    if sealed.is_empty() {
        bail!("{} has nothing in it to encrypt", a.file.display());
    }

    let out = a.output.unwrap_or_else(|| a.file.clone());
    file.record(
        TOOL,
        &format!("encrypted the payload to key slot {} ({})", a.slot, a.label),
        identity::load()?.as_ref(),
    )?;
    file.write(&out)?;

    println!(
        "{}  encrypted to slot {} ({})",
        out.display(),
        a.slot,
        a.label
    );
    println!(
        "  sealed: {}",
        sealed
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    // Say exactly what an interceptor still gets. A tool that lets someone
    // believe "encrypted" means "invisible" has misled them about the one
    // thing they were relying on it for.
    ctx.note(
        "still readable without the passphrase: that this is a Wick file, which format, \
         how big each chunk is, and the provenance chain — timestamps, what was done, and \
         which key signed it. The contents are not.",
    );
    ctx.note("nothing can recover this file if the passphrase is lost. There is no second way in.");
    Ok(())
}

fn cmd_decrypt(ctx: &Ctx, a: DecryptArgs) -> Result<()> {
    let mut file =
        WickFile::read(&a.file).with_context(|| format!("reading {}", a.file.display()))?;

    let locked = file.locked_slots();
    let slot = match (a.slot, locked.as_slice()) {
        (Some(s), _) => s,
        (None, [only]) => *only,
        (None, []) => bail!("{} is not encrypted", a.file.display()),
        // Two slots is split trust, and guessing which half the caller meant
        // is exactly the guess that would unseal the wrong one.
        (None, many) => bail!(
            "{} has {} encrypted slots ({}); say which with --slot",
            a.file.display(),
            many.len(),
            many.iter()
                .map(|s| format!("{s} {}", file.keys.label(*s)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    let pass = read_passphrase(
        &format!("passphrase for slot {slot} ({}): ", file.keys.label(slot)),
        a.passphrase_env.as_deref(),
    )?;
    file.unlock(slot, &pass)
        .with_context(|| format!("unlocking slot {slot}"))?;

    let label = file.keys.label(slot);
    let opened = file.unseal_payload(slot)?;
    file.remove_key_slot(slot)?;

    let out = a.output.unwrap_or_else(|| a.file.clone());
    file.record(
        TOOL,
        &format!("decrypted the payload from key slot {slot} ({label})"),
        identity::load()?.as_ref(),
    )?;
    file.write(&out)?;

    println!("{}  decrypted from slot {slot} ({label})", out.display());
    println!(
        "  opened: {}",
        opened
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    ctx.note("the file is plaintext again; anyone who can read it can read all of it");
    Ok(())
}

fn cmd_seal(ctx: &Ctx, a: SealArgs) -> Result<()> {
    let (tag, _) = libwick::sniff_path(&a.file)
        .ok_or_else(|| anyhow::anyhow!("{} is not a Wick file", a.file.display()))?;
    if tag != wick_emc::TAG {
        bail!(
            "sealing paths is a .emc operation; {} is a {tag} file",
            a.file.display()
        );
    }
    let mut file = WickFile::read(&a.file)?;

    let prompt = format!("passphrase for slot {} ({}): ", a.slot, a.label);
    let pass = read_passphrase(&prompt, a.passphrase_env.as_deref())?;
    // Only confirm when a human typed it. A scripted passphrase has no
    // typo to catch, and demanding it twice would just mean writing it
    // twice in the pipeline.
    if a.passphrase_env.is_none() && std::io::stdin().is_terminal() {
        let again = read_passphrase("repeat: ", None)?;
        if pass != again {
            bail!("passphrases do not match");
        }
    }
    if pass.chars().count() < 8 {
        bail!("use at least 8 characters — this is the only thing standing between the file and its secrets");
    }

    file.add_key_slot(a.slot, &a.label, &pass)?;
    let moved = wick_emc::seal_paths(&mut file, a.slot, &a.paths)?;
    if moved == 0 {
        bail!("none of those paths are in the file; nothing was sealed");
    }

    // Sealing every value still leaves the schema and the summary tier
    // describing them: field names, types, how many settings, what the
    // top-level keys are called. For a file meant to give up nothing, those
    // are the leak — so `--all` seals them too.
    let mut also = Vec::new();
    if a.all {
        for ty in [ChunkType::SCHM, ChunkType::SUMM] {
            if let Some(c) = file.chunks.get(ty).cloned() {
                file.chunks.set(c.sealed_to(a.slot));
                also.push(ty.to_string());
            }
        }
    }

    let what = if a.all {
        format!("sealed the whole config ({moved} value(s))")
    } else {
        format!("sealed {moved} value(s)")
    };
    file.record(
        TOOL,
        &format!("{what} to key slot {} ({})", a.slot, a.label),
        identity::load()?.as_ref(),
    )?;
    file.write(&a.file)?;

    println!("sealed {moved} value(s) into slot {} ({})", a.slot, a.label);
    if a.all {
        if !also.is_empty() {
            println!("also sealed: {}", also.join(", "));
        }
        // Never claim more than is true. The container is still a container.
        ctx.note(
            "the header, the chunk table and the provenance chain stay readable: anyone can \
             still see that this is a .emc file, how big each chunk is, and who edited it when",
        );
    } else {
        ctx.note("the rest of the file stays readable without the passphrase");
    }
    Ok(())
}

/// Require a `.emc`, and say so plainly when it is not one.
fn emc_file(path: &Path, what: &str) -> Result<WickFile> {
    let (tag, _) = libwick::sniff_path(path)
        .ok_or_else(|| anyhow::anyhow!("{} is not a Wick file", path.display()))?;
    if tag != wick_emc::TAG {
        bail!(
            "{what} is a .emc operation; {} is a {tag} file",
            path.display()
        );
    }
    WickFile::read(path).with_context(|| format!("reading {}", path.display()))
}

/// Read one value out of a config.
///
/// Bare text by default so it drops straight into a shell variable, JSON
/// with `--json` so a caller can tell `8080` from `"8080"`.
fn cmd_get(a: GetArgs) -> Result<()> {
    let mut file = emc_file(&a.file, "reading a path")?;
    for spec in &a.unlock {
        let (slot, pass) = match spec.split_once(':') {
            Some((s, p)) => (s.parse::<u8>()?, p.to_string()),
            None => {
                let slot: u8 = spec.parse()?;
                let label = file.keys.label(slot);
                (
                    slot,
                    read_passphrase(
                        &format!("passphrase for slot {slot} ({label}): "),
                        a.passphrase_env.as_deref(),
                    )?,
                )
            }
        };
        file.unlock(slot, &pass)?;
    }

    let Some(value) = wick_emc::get_path(&file, &a.path)? else {
        // A sealed value that is present but unreadable is a different
        // answer from one that is not there, and a script deciding whether
        // to write a default needs to know which it got.
        let locked = wick_emc::locked_paths(&file);
        if !locked.is_empty() {
            bail!(
                "no '{}' in the readable half of {}; slot {} ({}) is still sealed",
                a.path,
                name_of(&a.file),
                locked[0].0,
                locked[0].1
            );
        }
        bail!("no '{}' in {}", a.path, name_of(&a.file));
    };

    if a.json {
        json::emit(&serde_json::from_str::<serde_json::Value>(
            &wick_emc::value_json(&value)?,
        )?)?;
    } else {
        println!("{}", wick_emc::value_text(&value));
    }
    Ok(())
}

/// Set one value in place.
///
/// This exists for callers that are not people: a script or an agent making
/// a hundred small edits should not have to export a whole config, rewrite
/// it and import it back, and every such round trip is a chance to lose
/// something the exporter could not express.
fn cmd_set(ctx: &Ctx, a: SetArgs) -> Result<()> {
    let mut file = emc_file(&a.file, "setting a path")?;
    let value = wick_emc::parse_value(&a.value, a.string);

    // Refuse to shadow something that may already exist inside a sealed
    // group: two nodes claiming one path is a file that means different
    // things depending on who can read it.
    let locked = wick_emc::locked_paths(&file);
    if !locked.is_empty() && !a.force && wick_emc::get_path(&file, &a.path)?.is_none() {
        bail!(
            "{} has sealed values (slot {}, {}). If '{}' is one of them, setting it here would \
             create a second copy — unseal first, or pass --force to add it anyway",
            name_of(&a.file),
            locked[0].0,
            locked[0].1,
            a.path
        );
    }

    // Check the new value against the schema before writing, and say so
    // rather than leaving it for the next `hearth validate`. A caller making
    // a hundred edits wants to know which one was wrong while it still knows
    // what it was doing.
    let complaint = file.schema()?.and_then(|s| {
        s.field(&a.path)
            .map(|f| f.check(&value))
            .filter(|issues| !issues.is_empty())
            .map(|issues| issues[0].message.clone())
    });

    let was = wick_emc::set_path(&mut file, &a.path, value.clone())?;
    // The summary tier described the old payload. Rebuilding it is not
    // optional; a stale cheap tier is worse than none.
    if let Some(summ) = wick_emc::Emc.summarize(&file.data()?)? {
        file.set_summary(&summ)?;
    }
    file.record(
        TOOL,
        &match &was {
            Some(old) => format!("set {} ({} -> {})", a.path, old.preview(), value.preview()),
            None => format!("added {} = {}", a.path, value.preview()),
        },
        identity::load()?.as_ref(),
    )?;
    file.write(&a.file)?;

    match was {
        Some(old) => println!("{}  {} -> {}", a.path, old.preview(), value.preview()),
        None => {
            println!("{}  = {}", a.path, value.preview());
            ctx.note("that path was not in the file, so it was added");
        }
    }
    if let Some(why) = complaint {
        // Written, and reported. Refusing would make the schema a cage —
        // config does legitimately outgrow its inferred types — but writing
        // silently would make `hearth validate` a surprise later.
        eprintln!(
            "{} {}: {why} — `hearth validate` will report this",
            ctx.paint("33", "warning"),
            a.path
        );
    }
    Ok(())
}

fn cmd_unset(ctx: &Ctx, a: UnsetArgs) -> Result<()> {
    let mut file = emc_file(&a.file, "removing a path")?;
    let gone = wick_emc::unset_path(&mut file, &a.path)?;
    if gone == 0 {
        bail!("no '{}' in {}", a.path, name_of(&a.file));
    }
    if let Some(summ) = wick_emc::Emc.summarize(&file.data()?)? {
        file.set_summary(&summ)?;
    }
    file.record(
        TOOL,
        &format!("removed {gone} value(s) under {}", a.path),
        identity::load()?.as_ref(),
    )?;
    file.write(&a.file)?;
    println!("removed {gone} value(s) under {}", a.path);
    ctx.note("the provenance chain records what went, though not what it was");
    Ok(())
}

/// The inverse of `seal`. Without it, sealing a value means never editing
/// that file again, which makes the feature a trap rather than a tool.
fn cmd_unseal(ctx: &Ctx, a: UnsealArgs) -> Result<()> {
    let (tag, _) = libwick::sniff_path(&a.file)
        .ok_or_else(|| anyhow::anyhow!("{} is not a Wick file", a.file.display()))?;
    if tag != wick_emc::TAG {
        bail!(
            "unsealing paths is a .emc operation; {} is a {tag} file",
            a.file.display()
        );
    }

    let mut file = WickFile::read(&a.file)?;
    if !file.chunks.iter().any(|c| c.enc.slot != 0) {
        println!("{} has nothing sealed", a.file.display());
        return Ok(());
    }
    let label = file.keys.label(a.slot);
    let pass = read_passphrase(
        &format!("passphrase for slot {} ({label}): ", a.slot),
        a.passphrase_env.as_deref(),
    )?;
    file.unlock(a.slot, &pass)
        .with_context(|| format!("unlocking slot {}", a.slot))?;

    let back = wick_emc::unseal_paths(&mut file, &a.paths)?;
    if back == 0 {
        bail!("nothing sealed to slot {} matches those paths", a.slot);
    }

    // A schema or summary sealed by `seal --all` comes back with the values
    // it describes; leaving them locked would mean a readable config with an
    // unreadable description of itself.
    let mut also = Vec::new();
    for ty in [ChunkType::SCHM, ChunkType::SUMM] {
        if let Some(c) = file.chunks.get(ty) {
            if c.enc.slot == a.slot && !c.is_locked() {
                let mut opened = c.clone();
                opened.enc.slot = 0;
                file.chunks.set(opened);
                also.push(ty.to_string());
            }
        }
    }

    file.record(
        TOOL,
        &format!("unsealed {back} value(s) from key slot {}", a.slot),
        identity::load()?.as_ref(),
    )?;
    file.write(&a.file)?;

    println!("unsealed {back} value(s) from slot {} ({label})", a.slot);
    if !also.is_empty() {
        println!("also unsealed: {}", also.join(", "));
    }
    ctx.note(&format!(
        "those values are plaintext in {} again — `hearth seal` puts them back",
        name_of(&a.file)
    ));
    Ok(())
}

fn cmd_pin(ctx: &Ctx, a: PinArgs) -> Result<()> {
    let (tag, _) = libwick::sniff_path(&a.file)
        .ok_or_else(|| anyhow::anyhow!("{} is not a Wick file", a.file.display()))?;
    if tag != wick_emd::TAG {
        bail!(
            "pinning is a .emd operation; {} is a {tag} file",
            a.file.display()
        );
    }
    let mut file = WickFile::read(&a.file)?;
    let id = identity::load()?;

    if a.undo {
        if !wick_emd::unpin(&mut file)? {
            println!("{} was not pinned", a.file.display());
            return Ok(());
        }
        file.record(TOOL, "unpinned the layout", id.as_ref())?;
        file.write(&a.file)?;
        println!("layout unpinned; the document reflows again");
        return Ok(());
    }

    let setup = wick_emd::pdf_out::PageSetup::default();
    let n = wick_emd::pin(&mut file, &setup)?;
    file.record(TOOL, "pinned the layout", id.as_ref())?;
    file.write(&a.file)?;
    println!(
        "pinned {n} lines at {:.0}x{:.0} pt",
        setup.width, setup.height
    );
    ctx.note("exports now render from the stored positions, so they cannot drift between viewers");
    Ok(())
}

fn cmd_recompute(ctx: &Ctx, a: FileArg) -> Result<()> {
    let (tag, _) = libwick::sniff_path(&a.file)
        .ok_or_else(|| anyhow::anyhow!("{} is not a Wick file", a.file.display()))?;
    if tag != wick_emx::TAG {
        bail!(
            "computed columns are a .emx feature; {} is a {tag} file",
            a.file.display()
        );
    }
    let mut file = WickFile::read(&a.file)?;
    let mut t = wick_emx::table(&file)?;
    let computed = t.columns.iter().filter(|c| c.expr.is_some()).count();
    if computed == 0 {
        println!("no computed columns in {}", a.file.display());
        return Ok(());
    }

    // Units are checked here, before anything is written: a formula whose
    // dimensions do not match its column stops the command rather than
    // filling the table with plausible wrong numbers.
    let filled = t.recompute().map_err(anyhow::Error::msg)?;

    let data = t.encode()?;
    file.set_data(&data)?;
    if let Some(s) = wick_emx::Emx.summarize(&data)? {
        file.set_summary(&s)?;
    }
    file.record(
        TOOL,
        &format!("recomputed {computed} column(s)"),
        identity::load()?.as_ref(),
    )?;
    file.write(&a.file)?;

    println!("recomputed {computed} column(s), {filled} cells filled");
    ctx.note("units were checked against each column's declaration before anything was written");
    Ok(())
}

fn cmd_thumbnail(a: ThumbArgs) -> Result<()> {
    let (tag, _) = libwick::sniff_path(&a.file)
        .ok_or_else(|| anyhow::anyhow!("{} is not a Wick file", a.file.display()))?;
    if tag != wick_emi::TAG {
        bail!(
            "thumbnails come from .emi files; {} is a {tag} file",
            a.file.display()
        );
    }
    let file = WickFile::read(&a.file)?;
    let Some(png) = wick_emi::thumbnail_png(&file)? else {
        bail!(
            "{} has no summary tier to take a thumbnail from",
            a.file.display()
        );
    };
    let out = a
        .output
        .unwrap_or_else(|| a.file.with_extension("thumb.png"));
    std::fs::write(&out, &png)?;
    println!(
        "wrote {} ({} bytes, from the summary tier)",
        out.display(),
        png.len()
    );
    Ok(())
}

fn human(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
