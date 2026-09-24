//! Manual timing of the session index on a real codex home (R17).
//!
//! Reads `<codex home>/sessions` and `session_index.jsonl` **read-only**; the index cache goes
//! to a temporary directory, never to `~/.remuda`.
//!
//! ```sh
//! cargo run --release --example codex_timing -- [<codex home>]
//! ```
//! Default: `$HOME/.codex`.

use std::path::PathBuf;
use std::time::Instant;

use remuda::index::{self, Index};
use remuda::provider::Provider;
use remuda::registry::{Account, Home};

fn main() {
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let codex = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let account = Account {
        provider: Provider::Codex,
        name: "timing".into(),
        home: Home::Path(codex.display().to_string()),
    };
    let stores = index::stores(&[account], &remuda::Env::new());
    assert_eq!(stores.len(), 1, "no {}/sessions", codex.display());
    let tmp = tempfile::tempdir().expect("temp REMUDA_HOME");
    let cache = tmp.path().join("state/index.json");

    let t = Instant::now();
    let mut idx = Index::load(&cache);
    let cold = index::refresh(&mut idx, &stores, |_| {});
    let cold_refresh = t.elapsed();
    idx.save(&cache).expect("save");
    let cache_size = std::fs::metadata(&cache).map(|m| m.len()).unwrap_or(0);
    println!(
        "cold: refresh {cold_refresh:?} ({:.1} MB read, {cache_size} bytes cache); {cold:?}",
        cold.bytes_read as f64 / 1e6
    );
    for run in 1..=3 {
        let t = Instant::now();
        let mut idx = Index::load(&cache);
        let load = t.elapsed();
        let t = Instant::now();
        let warm = index::refresh(&mut idx, &stores, |_| {});
        let refresh = t.elapsed();
        idx.save(&cache).expect("save");
        println!("warm #{run}: load {load:?}, refresh {refresh:?}; {warm:?}");
    }

    let entries: Vec<_> = idx.entries.values().collect();
    let count = |f: &dyn Fn(&&index::Entry) -> bool| entries.iter().filter(|e| f(e)).count();
    let interactive = |e: &&index::Entry| matches!(e.source.as_deref(), Some("cli" | "vscode"));
    println!(
        "entries {}: thread name {}, first user text {}, any title {}, gap {}, no cwd {}",
        entries.len(),
        count(&|e| e.title.is_some()),
        count(&|e| e.first_user_text.is_some()),
        count(&|e| e.display_title().is_some()),
        count(&|e| e.gap),
        count(&|e| e.cwd_last.is_none()),
    );
    println!(
        "cli/vscode {}: any title {}; sources: {:?}",
        count(&interactive),
        count(&|e| interactive(e) && e.display_title().is_some()),
        {
            let mut m = std::collections::BTreeMap::new();
            for e in &entries {
                *m.entry(e.source.clone().unwrap_or("-".into())).or_insert(0) += 1;
            }
            m
        }
    );
}
