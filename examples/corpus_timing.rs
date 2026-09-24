//! Manual timing of the session index and history attribution on a real corpus (R8, R9).
//!
//! Reads the transcript store and `history.jsonl` **read-only**; the index cache goes to a
//! temporary directory, never to `~/.remuda`.
//!
//! ```sh
//! cargo run --release --example corpus_timing -- [<projects dir> [<history.jsonl>]]
//! ```
//! Defaults: `$HOME/.claude/projects` and `$HOME/.claude/history.jsonl`.

use std::path::PathBuf;
use std::time::Instant;

use remuda::attribution::Attribution;
use remuda::index::{self, Index, Store};
use remuda::provider::Provider;

fn main() {
    let home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let mut args = std::env::args().skip(1);
    let projects = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude/projects"));
    let history = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude/history.jsonl"));
    let store = Store {
        provider: Provider::Claude,
        path: projects.canonicalize().expect("projects dir"),
        accounts: vec!["claude:default".into()],
        thread_names: vec![],
    };
    let tmp = tempfile::tempdir().expect("temp REMUDA_HOME");
    let cache = tmp.path().join("state/index.json");

    let t = Instant::now();
    let mut idx = Index::load(&cache);
    let cold = index::refresh(&mut idx, std::slice::from_ref(&store), |_| {});
    let cold_refresh = t.elapsed();
    let t = Instant::now();
    idx.save(&cache).expect("save");
    let save = t.elapsed();
    let cache_size = std::fs::metadata(&cache).map(|m| m.len()).unwrap_or(0);
    println!("cold: refresh {cold_refresh:?}, save {save:?} ({cache_size} bytes cache); {cold:?}");

    for run in 1..=3 {
        let t = Instant::now();
        let mut idx = Index::load(&cache);
        let load = t.elapsed();
        let t = Instant::now();
        let warm = index::refresh(&mut idx, std::slice::from_ref(&store), |_| {});
        let refresh = t.elapsed();
        let t = Instant::now();
        idx.save(&cache).expect("save");
        let save = t.elapsed();
        let t = Instant::now();
        let sorted = idx.sorted().len();
        let sort = t.elapsed();
        println!(
            "warm #{run}: load {load:?}, refresh {refresh:?}, save {save:?}, sort {sort:?} \
             ({sorted} entries); {warm:?}"
        );
    }

    let with_title = idx.entries.values().filter(|e| e.title.is_some()).count();
    let with_text = idx
        .entries
        .values()
        .filter(|e| e.display_title().is_some())
        .count();
    let gaps = idx.entries.values().filter(|e| e.gap).count();
    let moved = idx
        .entries
        .values()
        .filter(|e| e.cwd_first.is_some() && e.cwd_first != e.cwd_last)
        .count();
    println!(
        "entries {}: ai-title {with_title}, any title {with_text}, head+tail only {gaps}, \
         cwd_first != cwd_last {moved}",
        idx.entries.len()
    );

    for run in 1..=3 {
        let t = Instant::now();
        let mut a = Attribution::default();
        a.add_history(&history, "claude:default");
        println!(
            "history #{run}: {:?} for {} bytes, {} sessions",
            t.elapsed(),
            std::fs::metadata(&history).map(|m| m.len()).unwrap_or(0),
            a.len()
        );
    }
    let mut a = Attribution::default();
    a.add_history(&history, "claude:default");
    let attributed = idx
        .entries
        .values()
        .filter(|e| !a.accounts(&e.session_id).is_empty())
        .count();
    println!(
        "entries attributable by this history.jsonl: {attributed} / {}",
        idx.entries.len()
    );
}
