//! Randomized check of the rules that keep mime from damaging files on disk.
//!
//! Random sequences of MCP calls — edits saved, held (`save: false`) and
//! rehearsed (with and without `view`), `undo_last`, Lisp checkpoints, reads,
//! explicit saves, closing the session — are mixed with writes to the file from
//! outside mime, and after every step three rules are checked:
//!
//! 1. A rehearsal, a held edit, a read and a failed call never touch the file.
//! 2. A change made outside mime is never overwritten silently: each outside
//!    write adds a marker, edits never touch markers, so a marker once on disk
//!    must stay there.
//! 3. A rehearsal leaves no trace beyond what a read leaves: the same sequence
//!    with each rehearsal replaced by a read gets the same answer and leaves
//!    the same file after every other step.
//!
//! A failure prints its seed and a failing sequence no single step can be
//! dropped from, as `mime call --script` lines. `MIME_DISK_SAFETY_CASES` sets
//! the number of cases (default 100); `MIME_DISK_SAFETY_SEED` runs one case.
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use mime_rs::rpc::{CallContext, Transport, call_tool, result_text};
use serde_json::{Value, json};

const FILE: &str = "doc.md";
const DEFAULT_CASES: u64 = 100;
const STEPS: usize = 40;

/// The one directory every case lives under, made the allowed root before the
/// first call (`$MIME_ROOTS` is read per call, and is process-wide).
fn root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("mime-disk-safety-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        let root = root.canonicalize().unwrap();
        // SAFETY: set once, before any call reads it; the only test in this
        // binary.
        unsafe { std::env::set_var("MIME_ROOTS", &root) };
        root
    })
}

/// xorshift64*: small, seedable, and the same on every machine.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn word(&mut self) -> String {
        format!("w{}", self.below(10))
    }
}

/// How an edit's result is kept.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Save,
    Hold,
    Rehearse,
    RehearseView,
}

impl Mode {
    fn apply(self, args: &mut Value) {
        match self {
            Mode::Save => {}
            Mode::Hold => args["save"] = json!(false),
            Mode::Rehearse => args["rehearse"] = json!(true),
            Mode::RehearseView => {
                args["rehearse"] = json!(true);
                args["view"] = json!(true);
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Step {
    Replace {
        pattern: String,
        replacement: String,
        all: bool,
        /// `expect_unique: false`: take the first of several matches rather
        /// than refuse (the default refuses a repeated pattern).
        first: bool,
        mode: Mode,
    },
    Insert {
        text: String,
        at_end: bool,
        mode: Mode,
    },
    Fill {
        column: u64,
        mode: Mode,
    },
    /// An edit made by a Lisp program.
    Program {
        word: String,
        mode: Mode,
    },
    Checkpoint(u64),
    Restore {
        label: u64,
        mode: Mode,
    },
    /// `times` undo_last calls in a row: one step reaches deep into the ring.
    Undo {
        hold: bool,
        times: u64,
    },
    Read,
    SaveBuffer,
    Close,
    /// A write from outside mime: the file is replaced (as git and formatters
    /// do) with marker `EXTERNAL-N` added at line `line`.
    Outside {
        marker: usize,
        line: u64,
    },
}

/// How often a case picks each kind of step, and each mode of an edit. Drawn
/// afresh for every case ("swarm testing"): a case heavy on saved edits and
/// undos, with no outside writes to clear the undo ring, reaches a full ring
/// that an even mix almost never does.
struct Mix {
    steps: [u64; 11],
    modes: [u64; 4],
}

impl Mix {
    fn pick(rng: &mut Rng) -> Mix {
        let mut steps = [0; 11].map(|_: u64| rng.below(6));
        let mut modes = [0; 4].map(|_: u64| rng.below(4));
        // Always some edits, some saved.
        steps[0] += 1;
        modes[0] += 1;
        Mix { steps, modes }
    }
}

/// An index into `weights`, drawn in proportion to them.
fn weighted(rng: &mut Rng, weights: &[u64]) -> usize {
    let mut n = rng.below(weights.iter().sum());
    for (i, &w) in weights.iter().enumerate() {
        if n < w {
            return i;
        }
        n -= w;
    }
    unreachable!("n is below the sum of the weights")
}

impl Step {
    fn pick(rng: &mut Rng, mix: &Mix, marker: &mut usize) -> Step {
        let mode = |rng: &mut Rng| {
            [Mode::Save, Mode::Hold, Mode::Rehearse, Mode::RehearseView][weighted(rng, &mix.modes)]
        };
        match weighted(rng, &mix.steps) {
            0 => Step::Replace {
                pattern: rng.word(),
                replacement: if rng.below(4) == 0 {
                    String::new()
                } else {
                    rng.word()
                },
                all: rng.below(3) == 0,
                first: rng.below(2) == 0,
                mode: mode(rng),
            },
            1 => Step::Insert {
                text: format!("{} ", rng.word()),
                at_end: rng.below(2) == 0,
                mode: mode(rng),
            },
            2 => Step::Fill {
                column: 12 + rng.below(30),
                mode: mode(rng),
            },
            3 => Step::Program {
                word: rng.word(),
                mode: mode(rng),
            },
            4 => Step::Checkpoint(rng.below(2)),
            5 => Step::Restore {
                label: rng.below(2),
                mode: mode(rng),
            },
            6 => Step::Undo {
                hold: rng.below(4) == 0,
                times: if rng.below(3) == 0 {
                    2 + rng.below(9)
                } else {
                    1
                },
            },
            7 => Step::Read,
            8 => Step::SaveBuffer,
            9 => Step::Close,
            10 => {
                *marker += 1;
                Step::Outside {
                    marker: *marker,
                    line: rng.below(8),
                }
            }
            _ => unreachable!("mix.steps has one weight per step kind"),
        }
    }

    /// How an edit step runs; `None` for every other step.
    fn mode(&self) -> Option<Mode> {
        match self {
            Step::Replace { mode, .. }
            | Step::Insert { mode, .. }
            | Step::Fill { mode, .. }
            | Step::Program { mode, .. }
            | Step::Restore { mode, .. } => Some(*mode),
            Step::Checkpoint(_)
            | Step::Undo { .. }
            | Step::Read
            | Step::SaveBuffer
            | Step::Close
            | Step::Outside { .. } => None,
        }
    }

    fn is_rehearsal(&self) -> bool {
        matches!(self.mode(), Some(Mode::Rehearse | Mode::RehearseView))
    }

    /// Whether a successful call may write the file: an edit or undo that is
    /// saved, and an explicit save. Everything else must leave it alone.
    fn may_write(&self) -> bool {
        match self {
            Step::Replace {
                pattern,
                replacement,
                mode,
                ..
            } => *mode == Mode::Save && pattern != replacement,
            Step::Insert { mode, .. }
            | Step::Fill { mode, .. }
            | Step::Program { mode, .. }
            | Step::Restore { mode, .. } => *mode == Mode::Save,
            Step::Undo { hold, .. } => !hold,
            Step::SaveBuffer => true,
            Step::Checkpoint(_) | Step::Read | Step::Close | Step::Outside { .. } => false,
        }
    }

    /// The MCP calls this step makes; none for an outside write.
    fn calls(&self, path: &str) -> Vec<(&'static str, Value)> {
        let times = match self {
            Step::Undo { times, .. } => *times as usize,
            _ => 1,
        };
        self.call(path).map_or_else(Vec::new, |c| vec![c; times])
    }

    fn call(&self, path: &str) -> Option<(&'static str, Value)> {
        let (name, mut args) = match self {
            Step::Replace {
                pattern,
                replacement,
                all,
                first,
                ..
            } => (
                "replace_text",
                if *first && !*all {
                    json!({ "pattern": pattern, "replacement": replacement, "expect_unique": false })
                } else {
                    json!({ "pattern": pattern, "replacement": replacement, "all": all })
                },
            ),
            Step::Insert { text, at_end, .. } => (
                "insert_text",
                json!({ "text": text, "pos": if *at_end { "eob" } else { "bob" } }),
            ),
            Step::Fill { column, .. } => ("fill_text", json!({ "all": true, "column": column })),
            Step::Program { word, .. } => (
                "run_program",
                json!({ "program": format!(
                    "(goto-char (point-min)) (forward-line 2) (insert \"{word} \")"
                ) }),
            ),
            Step::Checkpoint(label) => (
                "run_program",
                json!({ "program": format!("(checkpoint \"c{label}\")") }),
            ),
            Step::Restore { label, .. } => (
                "run_program",
                json!({ "program": format!("(restore-checkpoint \"c{label}\")") }),
            ),
            Step::Undo { hold, .. } => (
                "undo_last",
                if *hold {
                    json!({ "save": false })
                } else {
                    json!({})
                },
            ),
            Step::Read => ("view", json!({ "lines": [1, 100] })),
            Step::SaveBuffer => ("save_buffer", json!({})),
            Step::Close => ("close_session", json!({ "force": true })),
            Step::Outside { .. } => return None,
        };
        if let Some(mode) = self.mode() {
            mode.apply(&mut args);
        }
        args["path"] = json!(path);
        Some((name, args))
    }

    /// The step as `mime call --script` lines (an outside write as a comment).
    fn script_lines(&self) -> String {
        match self {
            Step::Outside { .. } => format!("# outside write: {self:?}"),
            _ => self
                .calls(FILE)
                .iter()
                .map(|(name, args)| json!({ "name": name, "arguments": args }).to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// The file as the rules see it: its bytes, and whether it was replaced or
/// rewritten (inode, mtime).
#[derive(PartialEq)]
struct Disk {
    bytes: Vec<u8>,
    ino: u64,
    mtime: (i64, i64),
}

fn disk(path: &Path) -> Disk {
    let meta = std::fs::metadata(path).expect("the file exists");
    Disk {
        bytes: std::fs::read(path).unwrap(),
        ino: meta.ino(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
    }
}

/// Whether `marker` is in `text` as a whole word: `EXTERNAL-1` is not in
/// `EXTERNAL-10`.
fn has_marker(text: &str, marker: &str) -> bool {
    text.match_indices(marker)
        .any(|(i, _)| !text[i + marker.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

fn outside_write(path: &Path, marker: usize, line: u64) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    let at = (line as usize).min(lines.len());
    let mark = format!("EXTERNAL-{marker}");
    lines.insert(at, &mark);
    let tmp = path.with_extension("outside");
    std::fs::write(&tmp, lines.join("\n") + "\n").unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

/// What a step answered, for comparing a run with its rehearsals left out.
#[derive(Debug, PartialEq)]
struct Seen {
    failed: bool,
    text: String,
    disk: String,
}

/// Run `steps` against a fresh copy of `initial` in `dir`, checking rules 1 and
/// 2 after every step. Returns what each step answered, or the broken rule.
fn run(dir: &Path, initial: &str, steps: &[Step]) -> Result<Vec<Seen>, String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(FILE);
    std::fs::write(&path, initial).unwrap();
    let path_str = path.to_str().unwrap().to_string();
    let dir_str = dir.to_str().unwrap();

    let (mut store, default) = mime_rs::mcp::stdio_store();
    let ctx = CallContext {
        transport: Transport::Stdio,
        implicit_workspace: Some(&default),
    };
    let mut call = |name: &str, args: Value| -> (bool, String) {
        let result = call_tool(name, args, &mut store, &ctx);
        // Whether a refused save calls the file replaced or modified depends on
        // the filesystem: an outside write's new file may get the inode number
        // an earlier one freed. Both refuse alike, so the answers say it alike.
        let text = result_text(&result)
            .replace(dir_str, "DIR")
            .replace("replaced on disk (new inode)", "changed on disk")
            .replace("modified on disk (mtime/size changed)", "changed on disk");
        (result["isError"] == true, text)
    };
    let mut markers: Vec<String> = Vec::new();
    let mut seen = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let before = disk(&path);
        // A step of several calls failed when all of them did; its answer is
        // theirs, one per line.
        let (failed, text) = if let Step::Outside { marker, line } = step {
            outside_write(&path, *marker, *line);
            markers.push(format!("EXTERNAL-{marker}"));
            (false, String::new())
        } else {
            let answers: Vec<(bool, String)> = step
                .calls(&path_str)
                .into_iter()
                .map(|(name, args)| call(name, args))
                .collect();
            let failed = answers.iter().all(|(failed, _)| *failed);
            let texts: Vec<String> = answers.into_iter().map(|(_, text)| text).collect();
            (failed, texts.join("\n"))
        };
        let after = disk(&path);
        let on_disk = String::from_utf8_lossy(&after.bytes).into_owned();
        let broke = |rule: &str| {
            format!(
                "step {} ({step:?}) broke rule {rule}\n--- answer ---\n{text}\n--- file ---\n{on_disk}",
                i + 1
            )
        };
        let outside = matches!(step, Step::Outside { .. });
        if !outside && (failed || !step.may_write()) && after != before {
            return Err(broke("1: the file changed on a call that must not write"));
        }
        if let Some(lost) = markers.iter().find(|m| !has_marker(&on_disk, m)) {
            return Err(broke(&format!(
                "2: the outside change {lost} was overwritten"
            )));
        }
        seen.push(Seen {
            failed,
            text,
            disk: on_disk,
        });
    }
    Ok(seen)
}

/// Run `steps` and check all three rules. For rule 3 each rehearsal is replaced
/// by a read, not dropped: a rehearsal opens the file and re-reads a clean
/// buffer whose file changed on disk, as a read does, and nothing else of it
/// may show afterwards.
fn check(dir: &Path, initial: &str, steps: &[Step]) -> Result<(), String> {
    let with = run(dir, initial, steps)?;
    let plain: Vec<Step> = steps
        .iter()
        .map(|s| {
            if s.is_rehearsal() {
                Step::Read
            } else {
                s.clone()
            }
        })
        .collect();
    let without = run(dir, initial, &plain)?;
    for (i, step) in steps.iter().enumerate() {
        if !step.is_rehearsal() && with[i] != without[i] {
            return Err(format!(
                "step {} ({step:?}) broke rule 3: it differs once the rehearsals are \
                 replaced by reads\n--- with ---\n{:?}\n--- without ---\n{:?}",
                i + 1,
                with[i],
                without[i]
            ));
        }
    }
    Ok(())
}

/// Drop steps one at a time while the sequence still fails; `failure` is what
/// the whole sequence failed with.
fn shrink(
    dir: &Path,
    initial: &str,
    mut steps: Vec<Step>,
    mut failure: String,
) -> (Vec<Step>, String) {
    loop {
        let mut smaller = false;
        let mut i = 0;
        while i < steps.len() {
            let mut fewer = steps.clone();
            fewer.remove(i);
            match check(dir, initial, &fewer) {
                Err(e) => {
                    steps = fewer;
                    failure = e;
                    smaller = true;
                }
                Ok(()) => i += 1,
            }
        }
        if !smaller {
            return (steps, failure);
        }
    }
}

fn case(seed: u64) -> (String, Vec<Step>) {
    let mut rng = Rng::new(seed);
    let mut initial = String::from("# Notes\n\n");
    for _ in 0..(3 + rng.below(4)) {
        let words: Vec<String> = (0..(2 + rng.below(5))).map(|_| rng.word()).collect();
        initial.push_str(&words.join(" "));
        initial.push_str(if rng.below(3) == 0 { "\n\n" } else { "\n" });
    }
    let mix = Mix::pick(&mut rng);
    let mut marker = 0;
    // Half the cases start by filling the undo ring (8 steps), which random
    // steps seldom do: outside writes and closes empty it.
    let fill = if rng.below(2) == 0 {
        8 + rng.below(3)
    } else {
        0
    };
    let mut steps: Vec<Step> = (0..fill)
        .map(|_| Step::Insert {
            text: format!("{} ", rng.word()),
            at_end: rng.below(2) == 0,
            mode: if rng.below(3) == 0 {
                Mode::Hold
            } else {
                Mode::Save
            },
        })
        .collect();
    steps.extend((0..STEPS).map(|_| Step::pick(&mut rng, &mix, &mut marker)));
    (initial, steps)
}

#[test]
fn random_call_sequences_keep_the_file_safe() {
    let root = root();
    let env = |key: &str| {
        std::env::var(key).ok().map(|v| {
            v.trim()
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("{key}={v:?} is not a number"))
        })
    };
    let seeds: Vec<u64> = match env("MIME_DISK_SAFETY_SEED") {
        Some(seed) => vec![seed],
        None => {
            let cases = env("MIME_DISK_SAFETY_CASES").unwrap_or(DEFAULT_CASES);
            assert!(cases > 0, "MIME_DISK_SAFETY_CASES=0 runs nothing");
            (1..=cases).collect()
        }
    };
    for seed in seeds {
        let dir = root.join(format!("case-{seed}"));
        let (initial, steps) = case(seed);
        if let Err(failure) = check(&dir, &initial, &steps) {
            let (steps, failure) = shrink(&dir, &initial, steps, failure);
            let script: Vec<String> = steps.iter().map(Step::script_lines).collect();
            panic!(
                "seed {seed} (MIME_DISK_SAFETY_SEED={seed} reruns it): {failure}\n\
                 --- initial {FILE} ---\n{initial}\n--- failing sequence ---\n{}",
                script.join("\n")
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    let _ = std::fs::remove_dir_all(root);
}
