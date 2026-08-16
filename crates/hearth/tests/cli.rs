//! End-to-end tests against the real binary.
//!
//! The unit tests in each crate check their own layer. These check the thing
//! a user actually runs: process in, files out, exit status. Round-trip
//! fidelity in particular is only meaningful when it is measured through the
//! whole pipeline, because that is where a converter loses data.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_hearth");

const NOTES_MD: &str = "\
# Field Notes

Measurements taken on the north ridge, 14 August. The wind was steady
and the instruments behaved.

## Method

- three passes per station
- discard the first, it is always warm

> The instrument is only as honest as the person reading it.

```python
def mean(xs):
    return sum(xs) / len(xs)
```

---

Next visit is scheduled for September.
";

const SERVICE_JSON: &str = r#"{
  "name": "ridge-logger",
  "listen": {"host": "0.0.0.0", "port": 8080},
  "database": {"host": "db.internal", "port": 5432, "password": "hunter2"},
  "features": ["telemetry", "retry"],
  "plugins": [],
  "debug": false
}"#;

const READINGS_CSV: &str = "\
station,distance (m),elapsed (s),sample_id,verified
north,1200,48,0071,true
ridge,3400,150,0072,true
saddle,,95,0073,false
";

/// A scratch directory per test, with its own signing key so that tests do
/// not read or write the developer's real identity.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!(
            "hearth-cli-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Sandbox { dir }
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// An executable stand-in for `$EDITOR`. `hearth edit` hands it the
    /// scratch file, so a script that appends to `$1` is an edit — which is
    /// the only way to test the command without a human at a keyboard.
    #[cfg(unix)]
    fn editor(&self, name: &str, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = self.write(name, &format!("#!/bin/sh\n{body}\n"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        // An absolute path: a relative program name is resolved against the
        // process's directory, not the child's, and that difference is a
        // platform detail no test should depend on.
        p.to_str().unwrap().to_string()
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap()
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(BIN)
            .args(args)
            .current_dir(&self.dir)
            .env("HEARTH_KEY_FILE", self.dir.join("id.key"))
            .env("COLUMNS", "100")
            .output()
            .expect("could not run hearth");
        Run::new(args, out)
    }

    fn run_env(&self, args: &[&str], key: &str, value: &str) -> Run {
        let out = Command::new(BIN)
            .args(args)
            .current_dir(&self.dir)
            .env("HEARTH_KEY_FILE", self.dir.join("id.key"))
            .env("COLUMNS", "100")
            .env(key, value)
            .output()
            .expect("could not run hearth");
        Run::new(args, out)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Run {
    args: String,
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn new(args: &[&str], out: Output) -> Run {
        Run {
            args: args.join(" "),
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        }
    }

    fn ok(self) -> Run {
        assert_eq!(
            self.code, 0,
            "`hearth {}` failed:\n{}\n{}",
            self.args, self.stdout, self.stderr
        );
        self
    }

    fn fails(self) -> Run {
        assert_ne!(
            self.code, 0,
            "`hearth {}` was expected to fail but succeeded:\n{}",
            self.args, self.stdout
        );
        self
    }

    fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    fn says(self, needle: &str) -> Run {
        assert!(
            self.all().contains(needle),
            "`hearth {}` did not mention {needle:?}:\n{}",
            self.args,
            self.all()
        );
        self
    }
}

/// A small PNG with a gradient, written without any image dependency.
fn gradient_png(w: u32, h: u32) -> Vec<u8> {
    png_of(w, h, |x, y| {
        [
            (x * 255 / w.max(1)) as u8,
            (y * 255 / h.max(1)) as u8,
            ((x + y) * 255 / (w + h).max(1)) as u8,
            255,
        ]
    })
}

/// A PNG of pseudo-random pixels. Where a gradient is the easiest thing in
/// the world to compress — `.emi` tiles are delta-filtered, so a gradient's
/// payload nearly vanishes — this is the hardest, which is what a test about
/// the size of one part of a file relative to another needs.
fn noise_png(w: u32, h: u32) -> Vec<u8> {
    png_of(w, h, |x, y| {
        let mut s = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (y as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        };
        [next(), next(), next(), 255]
    })
}

fn png_of(w: u32, h: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (i, e) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *e = c;
        }
        let mut c = 0xFFFF_FFFFu32;
        for b in bytes {
            c = table[((c ^ *b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        c ^ 0xFFFF_FFFF
    }
    fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        let mut body = tag.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&crc(&body).to_be_bytes());
        out
    }

    let mut raw = Vec::new();
    for y in 0..h {
        raw.push(0u8);
        for x in 0..w {
            raw.extend_from_slice(&pixel(x, y));
        }
    }

    let mut ihdr = w.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&chunk(b"IHDR", &ihdr));
    png.extend_from_slice(&chunk(b"IDAT", &deflate_stored(&raw)));
    png.extend_from_slice(&chunk(b"IEND", b""));
    png
}

/// zlib with stored (uncompressed) deflate blocks — enough for a fixture,
/// and it keeps the test crate free of a compression dependency.
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    for (i, block) in data.chunks(65_535).enumerate() {
        let last = (i + 1) * 65_535 >= data.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

// ---------------------------------------------------------------------------

#[test]
fn text_survives_a_round_trip_byte_for_byte() {
    let s = Sandbox::new("emt-roundtrip");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();
    s.run(&["convert", "notes.emt", "--to", "md", "-o", "back.md"])
        .ok();
    assert_eq!(s.read("back.md"), NOTES_MD);
}

#[test]
fn a_table_survives_a_round_trip_byte_for_byte() {
    let s = Sandbox::new("emx-roundtrip");
    s.write("readings.csv", READINGS_CSV);
    s.run(&["convert", "readings.csv"]).ok();
    s.run(&["convert", "readings.emx", "--to", "csv", "-o", "back.csv"])
        .ok();
    assert_eq!(s.read("back.csv"), READINGS_CSV);
    // The identifier column with leading zeros is the one that matters.
    assert!(s.read("back.csv").contains("0071"));
}

/// A CSV with enough rows to fill several `RGRP` groups, so a partial read
/// has something to leave behind.
fn wide_csv(rows: usize) -> String {
    let mut s = String::from("station,distance (m),elapsed (s),note\n");
    for i in 0..rows {
        s.push_str(&format!(
            "st{:05},{},{},note-{i}\n",
            i,
            100 + i,
            10 + i % 600
        ));
    }
    s
}

#[test]
fn viewing_the_first_rows_of_a_big_table_reads_one_row_group() {
    let s = Sandbox::new("emx-partial");
    s.write("big.csv", &wide_csv(5_000));
    s.run(&["convert", "big.csv"]).ok();

    // The rows shown are the file's first rows, and the count of what is
    // left is the file's, not the count of what was read.
    let out = s.run(&["view", "big.emx", "--limit", "3"]).ok();
    assert!(out.stdout.contains("st00000"), "{}", out.stdout);
    assert!(out.stdout.contains("st00002"), "{}", out.stdout);
    assert!(!out.stdout.contains("st00003"), "{}", out.stdout);
    assert!(out.stdout.contains("4997 more rows"), "{}", out.stdout);

    // A limit that spans a group boundary still gets contiguous rows.
    let out = s.run(&["view", "big.emx", "--limit", "600"]).ok();
    assert!(out.stdout.contains("st00511"), "{}", out.stdout);
    assert!(out.stdout.contains("st00512"), "{}", out.stdout);
    assert!(out.stdout.contains("st00599"), "{}", out.stdout);
    assert!(out.stdout.contains("4400 more rows"), "{}", out.stdout);

    // And asking for everything still gives everything.
    let out = s.run(&["view", "big.emx", "--limit", "5000"]).ok();
    assert!(out.stdout.contains("st04999"), "{}", out.stdout);
    assert!(!out.stdout.contains("more rows"), "{}", out.stdout);
}

#[test]
fn the_summary_tier_of_a_big_table_still_counts_every_row() {
    let s = Sandbox::new("emx-summary-partial");
    s.write("big.csv", &wide_csv(5_000));
    s.run(&["convert", "big.csv"]).ok();

    // The summary render never opens `DATA`, so nothing about the payload is
    // decoded — and the count it reports is still the file's.
    s.run(&["view", "big.emx", "--summary"])
        .ok()
        .says("5000 rows × 4 columns");
    s.run(&["preview", "big.emx", "--text"])
        .ok()
        .says("5000 rows");
}

#[test]
fn info_reports_where_each_row_group_lies_without_decoding_it() {
    let s = Sandbox::new("emx-index");
    s.write("big.csv", &wide_csv(2_000));
    s.run(&["convert", "big.csv"]).ok();

    s.run(&["info", "big.emx"])
        .ok()
        .says("sub-chunks")
        .says("RGRP");

    let out = s.run(&["info", "big.emx", "--json"]).ok();
    let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let payload = &v["payload"];
    assert_eq!(payload["addressable"], serde_json::json!(true));
    let kids = payload["children"].as_array().unwrap();
    // COLS, then 2000 rows in groups of 512.
    assert_eq!(kids.len(), 1 + 2000_usize.div_ceil(512));
    assert_eq!(kids[0]["type"], "COLS");
    assert_eq!(kids[1]["type"], "RGRP");
    // Every child sits inside the file and after the header.
    let size = v["size"].as_u64().unwrap();
    for k in kids {
        let (at, len) = (k["offset"].as_u64().unwrap(), k["length"].as_u64().unwrap());
        assert!(at >= 60 && at + len <= size, "child out of bounds: {k}");
    }
}

#[test]
fn a_payload_that_is_one_compressed_stream_says_so_rather_than_faking_an_index() {
    let s = Sandbox::new("emc-index");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();

    let out = s.run(&["info", "service.emc", "--json"]).ok();
    let v: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["payload"]["addressable"], serde_json::json!(false));
    assert_eq!(v["payload"]["why"], "compressed");
    assert!(v["payload"]["children"].is_null());
}

#[test]
fn a_partial_read_still_catches_damage_to_the_rows_it_never_decodes() {
    let s = Sandbox::new("emx-partial-hash");
    s.write("big.csv", &wide_csv(5_000));
    s.run(&["convert", "big.csv"]).ok();

    // Flip a byte near the end of the payload: well past the first row
    // group, which is all `--limit 3` will decode.
    let p = s.path("big.emx");
    let mut bytes = std::fs::read(&p).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xFF;
    std::fs::write(&p, &bytes).unwrap();

    s.run(&["view", "big.emx", "--limit", "3"])
        .fails()
        .says("content hash mismatch");
}

#[test]
fn config_survives_a_round_trip_with_its_key_order() {
    let s = Sandbox::new("emc-roundtrip");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();
    s.run(&["convert", "service.emc", "--to", "json", "-o", "back.json"])
        .ok();

    let back = s.read("back.json");
    for pair in ["name", "listen", "database", "features", "plugins", "debug"].windows(2) {
        let (a, b) = (
            back.find(&format!("\"{}\"", pair[0])).unwrap(),
            back.find(&format!("\"{}\"", pair[1])).unwrap(),
        );
        assert!(
            a < b,
            "key order changed: {} came after {}",
            pair[0],
            pair[1]
        );
    }
    assert!(
        back.contains("8080") && !back.contains("8080.0"),
        "an integer became a float"
    );
}

#[test]
fn an_image_survives_a_round_trip_pixel_for_pixel() {
    let s = Sandbox::new("emi-roundtrip");
    std::fs::write(s.path("swatch.png"), gradient_png(80, 50)).unwrap();
    s.run(&["convert", "swatch.png"]).ok();
    s.run(&["convert", "swatch.emi", "--to", "png", "-o", "back.png"])
        .ok();

    // Compare decoded pixels, not bytes: re-encoding chooses its own filters
    // and colour type, which is allowed as long as the image is identical.
    let a = decode_png(&std::fs::read(s.path("swatch.png")).unwrap());
    let b = decode_png(&std::fs::read(s.path("back.png")).unwrap());
    assert_eq!(a, b);
}

fn decode_png(bytes: &[u8]) -> Vec<u8> {
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    d.set_transformations(png::Transformations::normalize_to_color8());
    let mut r = d.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap_or(0)];
    let info = r.next_frame(&mut buf).unwrap();
    let n = (info.width * info.height) as usize;
    let stride = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        _ => panic!("unexpected colour type {:?}", info.color_type),
    };
    // Normalise to RGB so an alpha-dropping re-encode still compares equal.
    buf[..n * stride]
        .chunks_exact(stride)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect()
}

#[test]
fn info_reports_the_container_without_decoding_it() {
    let s = Sandbox::new("info");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();
    s.run(&["info", "notes.emt"])
        .ok()
        .says("MT (.emt, text)")
        .says("Wick v1.0")
        .says("provenance, summary")
        .says("full-fidelity payload")
        .says("summary / preview tier");
}

#[test]
fn the_summary_tier_renders_without_the_payload() {
    let s = Sandbox::new("summary");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();
    s.run(&["view", "notes.emt", "--summary"])
        .ok()
        .says("words")
        .says("Field Notes")
        .says("Method");
}

#[test]
fn diff_reports_the_setting_that_changed_and_exits_nonzero() {
    let s = Sandbox::new("diff");
    s.write("a.json", SERVICE_JSON);
    s.write("b.json", &SERVICE_JSON.replace("5432", "5433"));
    s.run(&["convert", "a.json"]).ok();
    s.run(&["convert", "b.json"]).ok();

    s.run(&["diff", "a.emc", "b.emc"])
        .fails()
        .says("database.port")
        .says("5432 -> 5433");
    // Conventional diff semantics: no differences means success.
    s.run(&["diff", "a.emc", "a.emc"])
        .ok()
        .says("no differences");
}

#[test]
fn diff_refuses_to_compare_different_formats() {
    let s = Sandbox::new("diff-mixed");
    s.write("a.json", SERVICE_JSON);
    s.write("b.csv", READINGS_CSV);
    s.run(&["convert", "a.json"]).ok();
    s.run(&["convert", "b.csv"]).ok();
    s.run(&["diff", "a.emc", "b.emx"])
        .fails()
        .says("cannot compare");
}

#[test]
fn unit_mismatched_arithmetic_fails_validation() {
    let s = Sandbox::new("units");
    s.write(
        "bad.csv",
        "distance (m),elapsed (s),nonsense (m) = distance + elapsed\n100,10,\n",
    );
    s.run(&["convert", "bad.csv"]).ok();
    s.run(&["validate", "bad.emx"]).fails().says("cannot add");
    s.run(&["recompute", "bad.emx"]).fails().says("cannot add");
}

#[test]
fn a_consistent_formula_computes() {
    let s = Sandbox::new("compute");
    s.write(
        "speeds.csv",
        "distance (km),elapsed (h),speed (km/h) = distance / elapsed\n120,2,\n90,1.5,\n",
    );
    s.run(&["convert", "speeds.csv"]).ok();
    s.run(&["recompute", "speeds.emx"])
        .ok()
        .says("2 cells filled");
    s.run(&["view", "speeds.emx"]).ok().says("60").says("60");
    s.run(&["validate", "speeds.emx"]).ok();
}

#[test]
fn split_trust_hides_one_half_and_refuses_to_export_it_blind() {
    let s = Sandbox::new("seal");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();
    s.run_env(
        &[
            "seal",
            "service.emc",
            "database.password",
            "--label",
            "prod",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "correct horse battery",
    )
    .ok()
    .says("sealed 1 value");

    s.run(&["info", "service.emc"]).ok().says("encrypted");

    // Without the passphrase: public config readable, secret absent, and the
    // exporter refuses rather than writing a config quietly missing it.
    let locked = s.run(&["view", "service.emc"]).ok();
    assert!(locked.stdout.contains("db.internal"));
    assert!(!locked.stdout.contains("hunter2"));
    s.run(&["convert", "service.emc", "--to", "json", "-o", "out.json"])
        .fails()
        .says("sealed");

    // With it: everything.
    s.run_env(
        &[
            "view",
            "service.emc",
            "--unlock",
            "1",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "correct horse battery",
    )
    .ok()
    .says("hunter2");

    s.run_env(
        &[
            "view",
            "service.emc",
            "--unlock",
            "1",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "wrong",
    )
    .fails()
    .says("wrong passphrase");
}

#[test]
fn a_flipped_byte_is_refused_rather_than_read() {
    let s = Sandbox::new("tamper");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();

    // Flip a byte inside the chunk table. The payload is compressed, so
    // there is no plaintext to target — which is the point: integrity does
    // not depend on being able to recognise the damage.
    let mut bytes = std::fs::read(s.path("notes.emt")).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xFF;
    std::fs::write(s.path("notes.emt"), &bytes).unwrap();

    s.run(&["view", "notes.emt"])
        .fails()
        .says("content hash mismatch");
    s.run(&["validate", "notes.emt"])
        .fails()
        .says("content hash mismatch");
}

#[test]
fn signed_provenance_verifies_and_names_the_key() {
    let s = Sandbox::new("provenance");
    s.run(&["key", "generate"]).ok().says("public key");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();

    s.run(&["verify-chain", "notes.emt"])
        .ok()
        .says("signed")
        .says("chain intact")
        .says("converted from legacy .md");
}

#[test]
fn unsigned_provenance_is_reported_as_such_not_as_valid() {
    let s = Sandbox::new("unsigned");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok().says("unsigned");
    s.run(&["verify-chain", "notes.emt"]).ok().says("0 signed");
}

#[test]
fn a_file_migrates_itself_using_its_own_embedded_rules() {
    let s = Sandbox::new("migrate");
    s.write("service.json", SERVICE_JSON);
    s.write(
        "rules.json",
        r#"{"rules": [{"from": 1, "to": 2, "note": "database moved under db",
            "ops": [
              {"op": "rename_key", "from": "database", "to": "db"},
              {"op": "set_default", "path": "db.pool", "value": 10},
              {"op": "drop_key", "path": "debug"}
            ]}]}"#,
    );
    s.run(&["convert", "service.json"]).ok();
    s.run(&["rules", "set", "service.emc", "rules.json"]).ok();
    s.run(&["rules", "show", "service.emc"])
        .ok()
        .says("v1 -> v2");

    s.run(&["migrate", "service.emc", "--dry-run"])
        .ok()
        .says("nothing written");
    // A dry run really is one.
    s.run(&["view", "service.emc"]).ok().says("database.host");

    s.run(&["migrate", "service.emc"]).ok().says("v1 -> v2");
    let after = s.run(&["view", "service.emc"]).ok();
    assert!(after.stdout.contains("db.host"));
    assert!(after.stdout.contains("db.pool"));
    assert!(!after.stdout.contains("database.host"));
    assert!(!after.stdout.contains("debug"));

    // The migration is recorded, and the file is still internally consistent.
    s.run(&["verify-chain", "service.emc"])
        .ok()
        .says("migrated payload schema");
    s.run(&["validate", "service.emc"]).ok();
}

#[test]
fn a_file_without_rules_says_so_rather_than_pretending() {
    let s = Sandbox::new("no-rules");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();
    s.run(&["migrate", "notes.emt"])
        .fails()
        .says("carries no MIGR rules");
}

#[test]
fn capabilities_are_checked_against_a_policy() {
    let s = Sandbox::new("caps");
    s.write("service.json", SERVICE_JSON);
    s.write(
        "caps.json",
        r#"{"network": true, "filesystem": ["write:/"], "env_read": ["AWS_SECRET"], "max_memory_mb": 4096}"#,
    );
    s.write(
        "policy.json",
        r#"{"network": false, "filesystem": ["read:./data"], "env_read": ["HOME"], "max_memory_mb": 256}"#,
    );
    s.run(&["convert", "service.json", "--caps", "caps.json"])
        .ok();

    // The declaration itself is linted even without a policy.
    s.run(&["validate", "service.emc"])
        .ok()
        .says("whole filesystem");

    s.run(&["validate", "service.emc", "--policy", "policy.json"])
        .fails()
        .says("policy does not permit")
        .says("file wants 4096 MB");
}

#[test]
fn a_pinned_document_renders_identically_every_time() {
    let s = Sandbox::new("pin");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md", "--as", "emd"]).ok();
    s.run(&["pin", "notes.emd"]).ok().says("pinned");

    s.run(&["convert", "notes.emd", "--to", "pdf", "-o", "one.pdf"])
        .ok();
    s.run(&["convert", "notes.emd", "--to", "pdf", "-o", "two.pdf"])
        .ok();
    assert_eq!(
        std::fs::read(s.path("one.pdf")).unwrap(),
        std::fs::read(s.path("two.pdf")).unwrap()
    );
    assert!(std::fs::read(s.path("one.pdf"))
        .unwrap()
        .starts_with(b"%PDF-1.4"));

    s.run(&["pin", "notes.emd", "--undo"]).ok().says("reflows");
}

#[test]
fn a_thumbnail_comes_from_the_summary_tier() {
    let s = Sandbox::new("thumb");
    // Big enough for the point to hold: the thumbnail's long edge is capped
    // at 128px whatever the image is, so what the tier saves grows with the
    // picture. On a small one it saves little and is not worth asserting.
    std::fs::write(s.path("swatch.png"), noise_png(600, 400)).unwrap();
    s.run(&["convert", "swatch.png"]).ok();
    s.run(&["thumbnail", "swatch.emi"])
        .ok()
        .says("from the summary tier");

    let thumb = std::fs::read(s.path("swatch.thumb.png")).unwrap();
    assert!(thumb.starts_with(b"\x89PNG"));
    let full = std::fs::metadata(s.path("swatch.emi")).unwrap().len() as usize;
    assert!(
        thumb.len() * 10 < full,
        "thumbnail {} vs file {full}",
        thumb.len()
    );
}

#[test]
fn ambiguous_input_picks_a_default_and_says_which() {
    let s = Sandbox::new("ambiguous");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"])
        .ok()
        .says("could also be imported as .emd");
    assert!(s.path("notes.emt").exists());

    s.run(&["convert", "notes.md", "--as", "emd", "-o", "doc.emd"])
        .ok();
    s.run(&["info", "doc.emd"]).ok().says("MD (.emd, document)");
}

#[test]
fn conversion_refuses_to_destroy_an_existing_file() {
    let s = Sandbox::new("overwrite");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();
    s.run(&["convert", "notes.md"]).fails().says("--force");
    s.run(&["convert", "notes.md", "--force"]).ok();
}

#[test]
fn an_unsupported_input_lists_what_is_supported() {
    let s = Sandbox::new("unsupported");
    s.write("thing.docx", "not really a docx");
    s.run(&["convert", "thing.docx"])
        .fails()
        .says("nothing imports '.docx'")
        .says(".csv");
}

#[test]
fn a_non_wick_file_is_rejected_clearly() {
    let s = Sandbox::new("foreign");
    s.write("random.emt", "this is not a Wick container");
    s.run(&["view", "random.emt"])
        .fails()
        .says("not a Wick file");
    s.run(&["info", "random.emt"])
        .fails()
        .says("not a Wick file");
}

#[test]
fn formats_lists_every_plugin_in_the_build() {
    let s = Sandbox::new("formats");
    let out = s.run(&["formats"]).ok();
    for ext in [".emt", ".emd", ".emi", ".emc", ".emx"] {
        assert!(
            out.stdout.contains(ext),
            "{ext} missing from `hearth formats`"
        );
    }
    assert!(out.stdout.contains("Wick v1.0"));
}

#[test]
fn naming_the_output_is_enough_to_choose_the_format() {
    // `convert <in> <out>` is what everybody types first. The output's
    // extension already says which format is wanted, so requiring `--as` as
    // well would be asking the same question twice.
    let s = Sandbox::new("convert-positional");
    s.write("notes.md", NOTES_MD);

    s.run(&["convert", "notes.md", "report.emd"]).ok();
    s.run(&["info", "report.emd"]).ok().says("document");
    // Without the name it would have gone to .emt, which is the default for
    // Markdown — so the output name really did decide.
    s.run(&["convert", "notes.md"]).ok();
    s.run(&["info", "notes.emt"]).ok().says("text");

    // And the same in the other direction: no --to needed.
    s.run(&["convert", "report.emd", "out.txt"]).ok();
    assert!(s.read("out.txt").contains("Field Notes"));

    // -o is the same thing, and still picks the format up from the name.
    s.run(&["convert", "notes.md", "-o", "flagged.emd"]).ok();
    s.run(&["info", "flagged.emd"]).ok().says("document");
}

#[test]
fn an_output_name_that_disagrees_with_the_flags_is_refused() {
    let s = Sandbox::new("convert-contradiction");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md", "out.emc", "--as", "emd"])
        .fails()
        .says("two different formats");
    // Importing produces an Ember file, so a legacy output name is a
    // direction mix-up rather than an unknown format.
    s.run(&["convert", "notes.md", "out.md"])
        .fails()
        .says("cannot be called .md")
        .says("--to md");
    s.run(&["convert", "notes.md", "a.emt", "-o", "b.emt"])
        .fails();
}

#[test]
fn an_empty_file_converts_into_a_valid_container() {
    // Nothing in, but the container is still a container: schema, summary
    // tier and a provenance chain that starts here.
    let s = Sandbox::new("convert-empty");
    s.write("empty.txt", "");
    s.run(&["convert", "empty.txt"]).ok();
    s.run(&["validate", "empty.emt"]).ok().says("ok");
    s.run(&["convert", "empty.emt", "--to", "txt", "-o", "back.txt"])
        .ok();
    assert_eq!(s.read("back.txt"), "");
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

#[test]
fn a_created_file_of_every_format_satisfies_its_own_rules() {
    // Creation goes through the same importer as conversion, so what this
    // really checks is that a starter is a legitimate document and not a
    // shape that only survives because nothing looks at it.
    let s = Sandbox::new("create-all");
    let created: [(&str, &[&str]); 5] = [
        ("notes.emt", &["--title", "Field Notes"]),
        ("report.emd", &["--title", "Report"]),
        ("service.emc", &[]),
        ("readings.emx", &["--columns", "station, distance (m)"]),
        ("swatch.emi", &["--size", "32x24"]),
    ];
    for (name, extra) in created {
        let mut args = vec!["create", name];
        args.extend_from_slice(extra);
        s.run(&args).ok();
        s.run(&["validate", name]).ok().says("ok");
        // Every created file carries the whole apparatus from the first
        // byte: a schema to check against, a cheap tier, and a history that
        // starts at its creation rather than at whatever touched it next.
        s.run(&["info", name])
            .ok()
            .says("SCHM")
            .says("SUMM")
            .says("PROV")
            .says("created a new");
    }
}

#[test]
fn create_names_the_file_after_the_format_when_the_path_does_not() {
    let s = Sandbox::new("create-as");
    s.run(&["create", "notes", "--as", "emt"]).ok();
    assert!(s.path("notes.emt").exists(), "notes.emt was not created");
    s.run(&["create", "nameless"]).fails().says("which format?");
}

#[test]
fn a_format_that_needs_to_be_told_something_asks_for_it() {
    // A table with no columns and an image with no size are not empty
    // documents, they are guesses waiting to be made. Both refuse.
    let s = Sandbox::new("create-needs");
    s.run(&["create", "readings.emx"])
        .fails()
        .says("needs its columns");
    s.run(&["create", "swatch.emi"])
        .fails()
        .says("needs its dimensions");
    s.run(&["create", "swatch.emi", "--size", "wide"])
        .fails()
        .says("640x480");
}

#[test]
fn an_option_that_means_nothing_to_a_format_is_an_error_not_a_no_op() {
    let s = Sandbox::new("create-stray");
    s.run(&["create", "notes.emt", "--columns", "a, b"])
        .fails()
        .says("--columns");
    s.run(&["create", "readings.emx", "--columns", "a", "--size", "8x8"])
        .fails()
        .says("--size");
}

#[test]
fn create_will_not_quietly_replace_an_existing_file() {
    let s = Sandbox::new("create-force");
    s.run(&["create", "notes.emt"]).ok();
    s.run(&["create", "notes.emt"]).fails().says("--force");
    s.run(&["create", "notes.emt", "--force"]).ok();
}

#[test]
fn a_created_table_carries_the_units_it_was_declared_with() {
    let s = Sandbox::new("create-units");
    s.run(&[
        "create",
        "journey.emx",
        "--columns",
        "distance (km), elapsed (min), speed (km/min) = distance / elapsed",
    ])
    .ok();
    // The unit check is the point of the format, so it has to be live in a
    // file that has never held a row.
    s.run(&["view", "journey.emx"]).ok().says("km/min");
    s.run(&["validate", "journey.emx"]).ok().says("ok");
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn an_edit_goes_out_to_a_legacy_format_and_comes_back_in() {
    let s = Sandbox::new("edit-emt");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();
    let ed = s.editor(
        "ed.sh",
        "printf '\\n## Addendum\\n\\nOne more pass.\\n' >> \"$1\"",
    );

    s.run(&["edit", "notes.emt", "--with", &ed])
        .ok()
        .says("change");
    s.run(&["view", "notes.emt"]).ok().says("Addendum");
    // Markdown went out and Markdown came back: the new heading is stored as
    // a heading, not as a paragraph that happens to start with a hash.
    s.run(&["view", "notes.emt"]).ok().says("h2");
    s.run(&["validate", "notes.emt"]).ok().says("ok");
}

#[cfg(unix)]
#[test]
fn an_edit_keeps_the_container_it_was_made_in() {
    let s = Sandbox::new("edit-keeps");
    s.write("service.json", SERVICE_JSON);
    s.write(
        "caps.json",
        r#"{"network": false, "filesystem": ["read:./data"], "max_memory_mb": 256}"#,
    );
    s.run(&["convert", "service.json", "--caps", "caps.json"])
        .ok();
    let ed = s.editor(
        "ed.sh",
        r#"sed 's/"port": 8080/"port": 9090/' "$1" > "$1.new" && mv "$1.new" "$1""#,
    );
    s.run(&["edit", "service.emc", "--with", &ed]).ok();

    // The edit replaced the payload. Everything else about the file is
    // still the same file: its capability declaration, and a provenance
    // chain that now has two links rather than one.
    s.run(&["view", "service.emc"]).ok().says("9090");
    s.run(&["info", "service.emc"]).ok().says("CAPS");
    s.run(&["verify-chain", "service.emc"])
        .ok()
        .says("2 entries")
        .says("edited as .json");
    s.run(&["validate", "service.emc"]).ok().says("ok");
}

#[cfg(unix)]
#[test]
fn an_edit_that_changes_nothing_writes_nothing() {
    // Rewriting the file would append a provenance entry saying an edit
    // happened, which would be a lie recorded in the one place that is
    // supposed to be trustworthy.
    let s = Sandbox::new("edit-noop");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();
    let before = std::fs::read(s.path("notes.emt")).unwrap();

    let ed = s.editor("ed.sh", "exit 0");
    s.run(&["edit", "notes.emt", "--with", &ed])
        .ok()
        .says("no changes");
    assert_eq!(before, std::fs::read(s.path("notes.emt")).unwrap());
}

#[cfg(unix)]
#[test]
fn an_edit_refuses_a_format_that_would_not_come_back_in() {
    let s = Sandbox::new("edit-target");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md", "--as", "emd"]).ok();
    let ed = s.editor("ed.sh", "exit 0");
    // .emd exports .pdf first, but an edit defaults to Markdown because a
    // PDF only comes back through best-effort text extraction.
    s.run(&["edit", "notes.emd", "--with", &ed])
        .ok()
        .says("as .md");
    s.run(&["edit", "notes.emd", "--to", "html", "--with", &ed])
        .fails()
        .says("can be edited as");
}

#[cfg(unix)]
#[test]
fn an_edit_is_refused_while_a_value_is_sealed() {
    let s = Sandbox::new("edit-sealed");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();
    s.run_env(
        &[
            "seal",
            "service.emc",
            "database.password",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "correct horse battery",
    )
    .ok();

    // Re-importing edited text cannot put back a secret that was never in
    // it. Refusing is the only answer that neither loses the value nor
    // writes it out in plaintext beside itself.
    let ed = s.editor("ed.sh", "printf 'x\\n' >> \"$1\"");
    s.run(&["edit", "service.emc", "--with", &ed])
        .fails()
        .says("sealed");
}

#[cfg(unix)]
#[test]
fn a_failing_editor_leaves_the_file_alone() {
    let s = Sandbox::new("edit-fails");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();
    let before = std::fs::read(s.path("notes.emt")).unwrap();

    let ed = s.editor("ed.sh", "exit 3");
    s.run(&["edit", "notes.emt", "--with", &ed])
        .fails()
        .says("exited");
    assert_eq!(before, std::fs::read(s.path("notes.emt")).unwrap());
}

#[cfg(unix)]
#[test]
fn a_created_file_can_be_edited_straight_away() {
    let s = Sandbox::new("create-edit");
    let ed = s.editor("ed.sh", "printf 'north,1200\\nridge,3400\\n' >> \"$1\"");
    s.run(&[
        "create",
        "readings.emx",
        "--columns",
        "station, distance (m)",
        "--edit",
        "--with",
        &ed,
    ])
    .ok();
    s.run(&["view", "readings.emx"]).ok().says("ridge");
    s.run(&["verify-chain", "readings.emx"])
        .ok()
        .says("created a new .emx")
        .says("edited as .csv");
}

#[test]
fn an_edit_can_be_split_in_half_for_a_front_end_to_drive() {
    // What the Hearth window does: ask for the editable form, hand it back
    // when the person saves. It has to be the same round trip the terminal
    // takes, or a save from a window would mean something different from a
    // save from an editor.
    let s = Sandbox::new("edit-halves");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();

    // The dialect goes to stdout, so the caller need not know the table.
    let export = s
        .run(&["edit", "notes.emt", "--export", "editable", "--quiet"])
        .ok();
    assert_eq!(export.stdout.trim(), "md");
    assert!(s.read("editable").contains("# Field Notes"));

    let edited = format!("{}\n## Addendum\n\nOne more pass.\n", s.read("editable"));
    s.write("editable", &edited);
    s.run(&["edit", "notes.emt", "--from", "editable"])
        .ok()
        .says("change");
    s.run(&["view", "notes.emt"]).ok().says("Addendum");
    s.run(&["verify-chain", "notes.emt"])
        .ok()
        .says("2 entries")
        .says("edited as .md");
}

#[test]
fn the_halves_of_an_edit_are_still_one_edit() {
    // Every rule the interactive path enforces has to hold on this path too,
    // or a front end would be a way around them.
    let s = Sandbox::new("edit-halves-rules");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();
    s.run_env(
        &[
            "seal",
            "service.emc",
            "database.password",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "correct horse battery",
    )
    .ok();
    s.run(&["edit", "service.emc", "--export", "x.json"])
        .fails()
        .says("sealed");
    s.write("x.json", "{\"a\": 1}");
    s.run(&["edit", "service.emc", "--from", "x.json"])
        .fails()
        .says("sealed");
}

// ---------------------------------------------------------------------------
// seal / unseal
// ---------------------------------------------------------------------------

/// Seal, unseal, and compare against what the file exported before either.
/// Round-tripping is the whole claim: if unsealing returned the same values
/// in a different order, every later diff would report an edit nobody made.
#[cfg(unix)]
#[test]
fn sealing_and_unsealing_returns_the_file_it_started_as() {
    let s = Sandbox::new("unseal-roundtrip");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();
    s.run(&["convert", "service.emc", "before.json"]).ok();

    s.run_env(
        &[
            "seal",
            "service.emc",
            "database.password",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "correct horse battery",
    )
    .ok();
    s.run_env(
        &[
            "unseal",
            "service.emc",
            "database.password",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "correct horse battery",
    )
    .ok()
    .says("unsealed 1 value");

    s.run(&["convert", "service.emc", "after.json"]).ok();
    assert_eq!(s.read("before.json"), s.read("after.json"));
}

#[test]
fn a_whole_config_can_be_sealed_and_gives_up_only_its_format() {
    let s = Sandbox::new("seal-all");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();
    s.run(&["convert", "service.emc", "before.json"]).ok();
    s.run_env(
        &["seal", "service.emc", "--all", "--passphrase-env", "PASS"],
        "PASS",
        "correct horse battery",
    )
    .ok()
    .says("SCHM")
    .says("SUMM");

    // Nothing about the contents is readable — not the values, and not the
    // key names that the schema and the summary tier would otherwise spell
    // out for anyone who opened the file.
    let view = s.run(&["view", "service.emc"]).ok();
    for leaked in ["ridge-logger", "database", "password", "8080"] {
        assert!(
            !view.stdout.contains(leaked),
            "a fully sealed config still shows {leaked:?}:\n{}",
            view.stdout
        );
    }
    assert!(view.all().contains("sealed"));

    // Sealed is not missing, and the difference is what tells somebody a
    // passphrase would help.
    s.run(&["view", "service.emc", "--summary"])
        .fails()
        .says("sealed to slot 1");
    s.run(&["validate", "service.emc"])
        .ok()
        .says("schema is sealed");

    // ...and it all comes back, in order.
    s.run_env(
        &["unseal", "service.emc", "--all", "--passphrase-env", "PASS"],
        "PASS",
        "correct horse battery",
    )
    .ok();
    s.run(&["convert", "service.emc", "after.json"]).ok();
    assert_eq!(
        s.read("before.json"),
        s.read("after.json"),
        "a fully sealed config came back in a different shape"
    );
    s.run(&["validate", "service.emc"]).ok().says("ok");
}

#[test]
fn unsealing_needs_the_right_passphrase_and_says_so() {
    let s = Sandbox::new("unseal-wrong");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();
    s.run_env(
        &[
            "seal",
            "service.emc",
            "database",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "correct horse battery",
    )
    .ok();

    s.run_env(
        &[
            "unseal",
            "service.emc",
            "database",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "wrong passphrase",
    )
    .fails()
    .says("slot 1");
    // The file is untouched by a failed attempt.
    s.run(&["view", "service.emc"]).ok().says("sealed");

    // A path that is not sealed is an error rather than a silent success.
    s.run_env(
        &[
            "unseal",
            "service.emc",
            "listen.port",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "correct horse battery",
    )
    .fails()
    .says("nothing sealed to slot 1 matches");
}

#[test]
fn unsealing_a_file_with_no_secrets_is_not_an_error() {
    let s = Sandbox::new("unseal-none");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();
    s.run(&["unseal", "service.emc", "--all"])
        .ok()
        .says("nothing sealed");
}

// ---------------------------------------------------------------------------
// machine-readable use
// ---------------------------------------------------------------------------

fn parse_json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON: {e}\n{text}"))
}

#[test]
fn json_output_is_the_only_thing_on_stdout() {
    // The point of --json is that a caller can pipe stdout straight into a
    // parser. One advisory line mixed in and that stops being true.
    let s = Sandbox::new("json-clean");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();

    for args in [
        vec!["info", "service.emc", "--json"],
        vec!["validate", "service.emc", "--json"],
        vec!["formats", "--json"],
    ] {
        let out = s.run(&args).ok();
        parse_json(&out.stdout);
    }

    let info = parse_json(&s.run(&["info", "service.emc", "--json"]).ok().stdout);
    assert_eq!(info["ext"], "emc");
    assert_eq!(info["tag"], "MC");
    assert_eq!(info["provenance"]["entries"], 1);
    assert!(info["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["type"] == "DATA"));

    let formats = parse_json(&s.run(&["formats", "--json"]).ok().stdout);
    assert_eq!(formats["formats"].as_array().unwrap().len(), 5);
}

#[test]
fn json_does_not_change_the_verdict_only_its_shape() {
    let s = Sandbox::new("json-exit");
    s.write("service.json", SERVICE_JSON);
    s.write("v2.json", &SERVICE_JSON.replace("8080", "9090"));
    s.run(&["convert", "service.json"]).ok();
    s.run(&["convert", "v2.json"]).ok();

    // diff still exits 1 when files differ, and 0 when they do not.
    let differ = s.run(&["diff", "service.emc", "v2.emc", "--json"]).fails();
    let d = parse_json(&differ.stdout);
    assert_eq!(d["identical"], false);
    assert!(d["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["path"].as_str().unwrap().contains("listen.port")));

    let same = s
        .run(&["diff", "service.emc", "service.emc", "--json"])
        .ok();
    assert_eq!(parse_json(&same.stdout)["identical"], true);

    // validate reports its issues and still fails when one is an error.
    s.write(
        "broken.csv",
        "distance (km),elapsed (min),speed (km/h) = distance + elapsed\n",
    );
    s.run(&["convert", "broken.csv"]).ok();
    let bad = s.run(&["validate", "broken.emx", "--json"]).fails();
    let v = parse_json(&bad.stdout);
    assert_eq!(v["ok"], false);
    assert!(v["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["severity"] == "error"));
}

#[test]
fn a_config_value_can_be_read_and_written_without_an_editor() {
    // The surgical edit: an agent making a hundred small changes should not
    // have to export, rewrite and re-import a file each time.
    let s = Sandbox::new("get-set");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();

    assert_eq!(
        s.run(&["get", "service.emc", "database.host"])
            .ok()
            .stdout
            .trim(),
        "db.internal"
    );
    // Bare text for a shell, JSON when the caller needs the type.
    assert_eq!(
        s.run(&["get", "service.emc", "listen.port"])
            .ok()
            .stdout
            .trim(),
        "8080"
    );

    s.run(&["set", "service.emc", "listen.port", "9090"]).ok();
    s.run(&["set", "service.emc", "database.host", "db2.internal"])
        .ok();
    s.run(&["set", "service.emc", "features.0", "metrics"]).ok();
    s.run(&["set", "service.emc", "new.setting", "true"]).ok();

    assert_eq!(
        s.run(&["get", "service.emc", "listen.port"])
            .ok()
            .stdout
            .trim(),
        "9090"
    );
    assert_eq!(
        s.run(&["get", "service.emc", "new.setting"])
            .ok()
            .stdout
            .trim(),
        "true"
    );

    // A number stays a number, and --string forces the other reading — but
    // the schema said int, so the write is reported rather than silent, and
    // `validate` agrees with the warning afterwards.
    s.run(&["set", "service.emc", "listen.port", "9091", "--string"])
        .ok()
        .says("declared int, found string");
    assert_eq!(
        s.run(&["get", "service.emc", "listen.port", "--json"])
            .ok()
            .stdout
            .trim(),
        "\"9091\""
    );
    s.run(&["validate", "service.emc"])
        .fails()
        .says("declared int, found string");
    s.run(&["set", "service.emc", "listen.port", "9091"]).ok();
    s.run(&["validate", "service.emc"]).ok().says("ok");

    s.run(&["unset", "service.emc", "database"])
        .ok()
        .says("removed 3");
    s.run(&["get", "service.emc", "database.host"])
        .fails()
        .says("no 'database.host'");

    // Every one of those was an edit, and the file still adds up.
    s.run(&["validate", "service.emc"]).ok().says("ok");
    let info = parse_json(&s.run(&["info", "service.emc", "--json"]).ok().stdout);
    assert_eq!(info["provenance"]["entries"], 8);
}

#[test]
fn setting_a_new_path_beside_sealed_values_is_refused_by_default() {
    // Two nodes claiming one path is a file that means different things
    // depending on who can read it.
    let s = Sandbox::new("set-sealed");
    s.write("service.json", SERVICE_JSON);
    s.run(&["convert", "service.json"]).ok();
    s.run_env(
        &[
            "seal",
            "service.emc",
            "database",
            "--passphrase-env",
            "PASS",
        ],
        "PASS",
        "correct horse battery",
    )
    .ok();

    s.run(&["set", "service.emc", "database.password", "guess"])
        .fails()
        .says("--force");
    // A path in the readable half is still ordinary work.
    s.run(&["set", "service.emc", "listen.port", "9090"]).ok();
}

#[cfg(unix)]
#[test]
fn conversion_works_in_a_pipeline() {
    // No temporary files: `-` is stdin and `-` is stdout, so hearth can sit
    // in the middle of a pipe like any other filter.
    let s = Sandbox::new("pipes");
    let script = s.editor(
        "pipe.sh",
        "printf '# Piped\\n\\nStraight through.\\n' | \"$1\" -q convert - --src md -o - | \
         \"$1\" -q convert - --to md -o - > out.md",
    );
    let out = Command::new(&script)
        .arg(BIN)
        .current_dir(&s.dir)
        .output()
        .expect("pipeline failed to run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(s.read("out.md"), "# Piped\n\nStraight through.\n");
}

#[test]
fn reading_standard_input_needs_to_be_told_what_it_is() {
    // Bytes on stdin carry no name. Guessing would mean a file whose format
    // depends on what the sniffer happened to think.
    let s = Sandbox::new("stdin-src");
    s.run(&["convert", "-", "out.emt"]).fails().says("--src");
}

// ---------------------------------------------------------------------------
// preview
// ---------------------------------------------------------------------------

#[test]
fn a_preview_reads_the_summary_tier_and_says_which_tier_it_read() {
    let s = Sandbox::new("preview");
    s.write("notes.md", NOTES_MD);
    s.run(&["convert", "notes.md"]).ok();

    let html = s.run(&["preview", "notes.emt"]).ok();
    assert!(html.stdout.starts_with("<!doctype html>"), "not HTML");
    for needle in ["notes.emt", "summary tier", "Field Notes", "PROV"] {
        assert!(
            html.stdout.contains(needle),
            "the preview does not mention {needle:?}"
        );
    }
    // --full is the escape hatch, and it has to be honest about being one.
    s.run(&["preview", "notes.emt", "--full"])
        .ok()
        .says("read from the payload");
    s.run(&["preview", "notes.emt", "--text"])
        .ok()
        .says("Field Notes");
}

#[test]
fn a_preview_of_an_image_carries_the_picture_with_it() {
    // A Quick Look pane is sandboxed and cannot fetch anything, so the
    // picture has to be in the page itself.
    let s = Sandbox::new("preview-emi");
    std::fs::write(s.path("swatch.png"), gradient_png(80, 50)).unwrap();
    s.run(&["convert", "swatch.png"]).ok();
    let html = s.run(&["preview", "swatch.emi"]).ok();
    assert!(
        html.stdout.contains("src=\"data:image/png;base64,"),
        "no embedded picture"
    );
    assert!(
        !html.stdout.contains("http://") && !html.stdout.contains("https://"),
        "the preview references something it cannot reach"
    );
}

#[test]
fn a_preview_shows_the_image_rather_than_stretching_its_thumbnail() {
    // The summary thumbnail is 128px on its long edge. Embedding that alone
    // and letting an 800pt pane stretch it is what made a converted image
    // look worse than the PNG it came from, so a preview of an image this
    // size draws every pixel of it, at its own size.
    let s = Sandbox::new("preview-sharp");
    std::fs::write(s.path("wide.png"), gradient_png(400, 300)).unwrap();
    s.run(&["convert", "wide.png"]).ok();
    let html = s.run(&["preview", "wide.emi"]).ok();
    assert!(
        html.stdout.contains("width=\"400\" height=\"300\""),
        "the picture is not at its natural size"
    );
    assert!(
        html.stdout.contains("400×300, drawn from the payload"),
        "the page does not say where the picture came from"
    );
    // And having decoded it, the page stops claiming it did not.
    assert!(
        !html.stdout.contains("the payload was never decoded"),
        "the tier line contradicts the picture above it"
    );
}

#[test]
fn a_preview_cannot_be_talked_into_running_a_file_s_contents() {
    let s = Sandbox::new("preview-escape");
    s.write("evil.md", "# <script>alert('x')</script>\n\nhello\n");
    s.run(&["convert", "evil.md"]).ok();
    let html = s.run(&["preview", "evil.emt", "--full"]).ok();
    assert!(
        !html.stdout.contains("<script>alert"),
        "markup from the file reached the page unescaped"
    );
    assert!(html.stdout.contains("&lt;script&gt;"));
}

#[test]
fn every_example_in_the_repository_converts() {
    // The examples are what a newcomer runs first. If one of them stops
    // working, that is the worst possible test to have missing.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("examples");
    if !root.exists() {
        return;
    }
    let s = Sandbox::new("examples");
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        // policy.json and caps.json are inputs to other commands, not
        // documents to convert.
        if name.starts_with("policy") || name.starts_with("caps") {
            continue;
        }
        std::fs::copy(&path, s.path(&name)).unwrap();
        s.run(&["convert", &name]).ok();
    }
}
