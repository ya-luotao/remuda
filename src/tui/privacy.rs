//! Private mode (SPEC R21): what the TUI shows while it is on. [`redacted`] makes a copy of
//! the [`App`] with account names aliased and emails, organizations, paths and session
//! content masked, and the screen is drawn from that copy only. Every struct and enum it
//! copies is destructured without `..`: a field added later does not compile until it is
//! decided how private mode shows it.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::checks::Check;
use crate::identity::Identity;
use crate::index::{Entry, Index, Store};
use crate::launch;
use crate::live::{LiveId, LiveSession};
use crate::provider::Provider;
use crate::registry::{Account, DEFAULT_NAME, Home};
use crate::stats::{self, ModelRow, Report, Section, Table};
use crate::transcript::Message;
use crate::usage::{CachedUsage, Resets, UsageRow};

use super::app::{
    AccountState, App, Confirm, Field, Form, FormKind, History, LaunchRequest, Logs, Mask, Notice,
    Overlay, Pick, Preview, ResumeCodex, StatsState,
};

/// What masked text shows.
pub const MASK: &str = "•••";
/// What a masked email shows.
pub const MASKED_EMAIL: &str = "•••@•••";

/// `p` with each component shown as [`MASK`], and a leading `home` (or `~`) as `~`:
/// `/Users/you/space/remuda` is `~/•••/•••` when `home` is `/Users/you`, `/tmp` is `/•••`.
pub fn mask_path(p: &str, home: Option<&str>) -> String {
    let home = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| !h.is_empty());
    let under_home = home.and_then(|h| match p.strip_prefix(h) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => Some(rest),
        _ => None,
    });
    let (root, rest) = match under_home {
        Some(rest) => ("~", rest),
        None => match p.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => ("~", rest),
            _ if p.starts_with('/') => ("/", p),
            _ => ("", p),
        },
    };
    let masked: Vec<&str> = rest
        .split('/')
        .filter(|c| !c.is_empty())
        .map(|_| MASK)
        .collect();
    match (root, masked.is_empty()) {
        ("~", true) => "~".to_string(),
        ("~", false) => format!("~/{}", masked.join("/")),
        ("/", _) => format!("/{}", masked.join("/")),
        _ => masked.join("/"),
    }
}

/// A path that stands for `p` where a path is only a key (the index, the preview, the
/// selected session): unique in practice, and nothing of `p` shows.
pub fn key_path(p: &Path) -> PathBuf {
    PathBuf::from(format!(
        "/private/{:016x}",
        stats::fnv1a(&[p.as_os_str().as_bytes()])
    ))
}

/// Aliases of account names (`claude:max` → `claude:account-2`), per provider in the order the
/// accounts were first noted; `default` stays itself. Grow-only: an alias never changes while
/// the TUI runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aliases {
    /// `provider:name` → `provider:account-<n>`.
    map: BTreeMap<String, String>,
    /// Aliases given so far, per provider.
    given: BTreeMap<String, usize>,
}

impl Aliases {
    /// Gives `qualified` (`provider:name`) the next alias of its provider, unless it has one.
    pub fn note(&mut self, qualified: &str) {
        if self.map.contains_key(qualified) {
            return;
        }
        let Some((provider, name)) = qualified.split_once(':') else {
            return;
        };
        let alias = if name == DEFAULT_NAME {
            qualified.to_string()
        } else {
            let n = self.given.entry(provider.to_string()).or_default();
            *n += 1;
            format!("{provider}:account-{n}")
        };
        self.map.insert(qualified.to_string(), alias);
    }

    /// The alias of `qualified`; one never noted is `<provider>:account-?` (`account-?` when
    /// the prefix is not a provider). Never the name itself, unless it is `default`.
    pub fn qualified(&self, qualified: &str) -> String {
        if let Some(alias) = self.map.get(qualified) {
            return alias.clone();
        }
        match qualified.split_once(':') {
            Some((provider, name)) if Provider::parse(provider).is_some() => {
                if name == DEFAULT_NAME {
                    qualified.to_string()
                } else {
                    format!("{provider}:account-?")
                }
            }
            _ => "account-?".to_string(),
        }
    }

    /// The alias without its provider prefix (`account-2`).
    pub fn name(&self, qualified: &str) -> String {
        let alias = self.qualified(qualified);
        match alias.split_once(':') {
            Some((_, name)) => name.to_string(),
            None => alias,
        }
    }

    /// Every noted name with its alias.
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.map.iter().map(|(q, a)| (q.as_str(), a.as_str()))
    }
}

/// Masks free text (notices, errors, check messages, a launch's description; R21), knowing the
/// app's names and secrets.
pub struct Scrubber {
    home: Option<String>,
    /// Exact text and what it becomes, longest first.
    secrets: Vec<(String, String)>,
    /// Paths the app knows (`$HOME`, homes, stores, the start directory, a typed directory):
    /// a path starts wherever one of them does, not only at the start of a word.
    paths: Vec<String>,
    /// Account names (qualified, then bare) and their aliases, longest first within each.
    names: Vec<(String, String)>,
}

/// Secrets shorter than this are not replaced where they occur in free text: a one-letter
/// organization would mangle every word.
const MIN_SECRET: usize = 2;

impl Scrubber {
    pub fn of(app: &App) -> Scrubber {
        let home = app.home.clone();
        let mut secrets: Vec<(String, String)> = Vec::new();
        let mut secret = |text: &str, shown: String| {
            if text.chars().count() >= MIN_SECRET {
                secrets.push((text.to_string(), shown));
            }
        };
        let mut paths: Vec<String> = Vec::new();
        let mut path = |p: &str| {
            let p = p.trim_end_matches('/');
            if p.len() >= MIN_SECRET && p.starts_with('/') {
                paths.push(p.to_string());
            }
        };
        path(home.as_deref().unwrap_or_default());
        for a in &app.accounts {
            if let Home::Path(p) = &a.account.home {
                path(p);
            }
            if let Some(Identity::LoggedIn { email, org, .. }) = &a.identity {
                if let Some(e) = email {
                    secret(e, MASKED_EMAIL.to_string());
                }
                if let Some(o) = org {
                    secret(o, MASK.to_string());
                }
            }
        }
        for s in &app.live {
            if let Some(name) = &s.name {
                secret(name, MASK.to_string());
            }
        }
        for store in app.stores.iter().flatten() {
            path(&store.path.to_string_lossy());
        }
        if let Some(cwd) = &app.cwd {
            path(&cwd.to_string_lossy());
        }
        secret(&app.history.query, MASK.to_string());
        if let Some(Overlay::Form(form)) = &app.overlay {
            for field in &form.fields {
                match field.mask {
                    Mask::Path => path(&field.value),
                    Mask::Text | Mask::Email => {
                        secret(&field.value, masked_value(field, home.as_deref()));
                    }
                    Mask::Plain => {}
                }
            }
        }
        secrets.sort_by_key(|(text, _)| std::cmp::Reverse(text.len()));
        paths.sort_by_key(|p| std::cmp::Reverse(p.len()));

        let mut qualified: Vec<(String, String)> = Vec::new();
        let mut bare: Vec<(String, String)> = Vec::new();
        for (name, alias) in app.aliases.pairs() {
            let Some((_, short)) = name.split_once(':') else {
                continue;
            };
            if short == DEFAULT_NAME {
                continue;
            }
            qualified.push((name.to_string(), alias.to_string()));
            let alias_short = alias.split_once(':').map_or(alias, |(_, a)| a);
            if !bare.iter().any(|(b, _)| b == short) {
                bare.push((short.to_string(), alias_short.to_string()));
            }
        }
        qualified.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
        bare.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
        qualified.extend(bare);
        Scrubber {
            home,
            secrets,
            paths,
            names: qualified,
        }
    }

    /// `text` with, in order: `“…”` masked; the app's secrets (emails, organizations, live
    /// session names, typed values, the search) masked; each path masked; each word with an
    /// `@` masked; account names replaced by their aliases, as whole words.
    pub fn text(&self, text: &str) -> String {
        let mut out = mask_quoted(text);
        for (secret, shown) in &self.secrets {
            out = out.replace(secret.as_str(), shown);
        }
        let out = self.mask_paths(&out);
        let out = mask_emails(&out);
        self.alias_names(&out)
    }

    /// Each path, to the next `: `, `, `, `; `, quote, bracket, or the end: from a `/` or `~/`
    /// that begins a word, or from wherever a path the app knows starts, as a whole component
    /// (`$HOME` = `/Users/you` does not start one in `/Users/yours`).
    fn mask_paths(&self, text: &str) -> String {
        const OPENERS: [char; 6] = ['(', '[', '"', '\'', '“', '='];
        const STOPS: [&str; 9] = [": ", ", ", "; ", "\"", "'", "”", ")", "]", "\n"];
        let component_ends = |after: &str| {
            after
                .chars()
                .next()
                .is_none_or(|c| c == '/' || c.is_whitespace() || ":,;\"'”)]".contains(c))
        };
        let mut out = String::new();
        let mut rest = text;
        let mut prev: Option<char> = None;
        while let Some(c) = rest.chars().next() {
            let begins = prev.is_none_or(|p| p.is_whitespace() || OPENERS.contains(&p))
                && (c == '/' || rest.starts_with("~/"));
            let known = self
                .paths
                .iter()
                .any(|p| rest.strip_prefix(p.as_str()).is_some_and(&component_ends));
            if begins || known {
                let end = STOPS
                    .iter()
                    .filter_map(|s| rest.find(s))
                    .min()
                    .unwrap_or(rest.len());
                let (path, after) = rest.split_at(end);
                out.push_str(&mask_path(path, self.home.as_deref()));
                prev = path.chars().next_back();
                rest = after;
                continue;
            }
            out.push(c);
            prev = Some(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    /// Account names at word boundaries, in one pass: an alias is never replaced again.
    fn alias_names(&self, text: &str) -> String {
        let word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
        let mut out = String::new();
        let mut rest = text;
        let mut prev: Option<char> = None;
        'scan: while let Some(c) = rest.chars().next() {
            if prev.is_none_or(|p| !word(p)) {
                for (name, alias) in &self.names {
                    if let Some(after) = rest.strip_prefix(name.as_str())
                        && after.chars().next().is_none_or(|n| !word(n))
                    {
                        out.push_str(alias);
                        prev = alias.chars().next_back();
                        rest = after;
                        continue 'scan;
                    }
                }
            }
            out.push(c);
            prev = Some(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }
}

/// Text between `“` and `”` (or the end) as [`MASK`].
fn mask_quoted(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('“') {
        out.push_str(&rest[..open + '“'.len_utf8()]);
        let inner = &rest[open + '“'.len_utf8()..];
        out.push_str(MASK);
        match inner.find('”') {
            Some(close) => rest = &inner[close..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Each run of non-blank characters holding an `@` as [`MASKED_EMAIL`].
fn mask_emails(text: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.contains('@') {
            out.push_str(MASKED_EMAIL);
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_whitespace() {
            flush(&mut word, &mut out);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// A form field's value as private mode shows it.
fn masked_value(field: &Field, home: Option<&str>) -> String {
    let Field {
        label: _,
        value,
        mask,
    } = field;
    if value.is_empty() {
        return String::new();
    }
    match mask {
        Mask::Path => mask_path(value, home),
        Mask::Text => MASK.to_string(),
        Mask::Email => MASKED_EMAIL.to_string(),
        Mask::Plain => value.clone(),
    }
}

/// The copy of `app` that private mode draws (R21). Only what the screen may show is left:
/// account names become aliases, emails, organizations, paths, titles, first messages, session
/// names, previews, logs, the search and typed values are masked, free text is scrubbed, and
/// paths that serve only as keys become [`key_path`]s consistently, so lookups between the
/// copied fields still agree.
pub fn redacted(app: &App) -> App {
    let r = Redactor {
        scrub: Scrubber::of(app),
        aliases: &app.aliases,
        home: app.home.as_deref(),
    };
    let App {
        mode,
        now,
        cwd,
        tz,
        home: _,
        size,
        view,
        help,
        accounts,
        accounts_list,
        checks,
        checks_in_flight,
        index,
        indexing,
        index_in_flight,
        index_loaded,
        index_refreshed,
        index_error,
        by_session,
        attribution_base,
        attribution,
        attribution_in_flight,
        live,
        live_rows,
        live_show_inactive,
        live_list,
        logs,
        live_loaded,
        live_in_flight,
        live_stale,
        live_updated,
        history,
        preview,
        stats,
        notice,
        overlay,
        stores,
        pending,
        launch_checks,
        cancelled,
        form_check,
        index_again,
        attribution_again,
        live_again,
        private,
        aliases: _,
    } = app;
    App {
        mode: *mode,
        now: *now,
        cwd: cwd.as_deref().map(|p| r.path_buf(p)),
        tz: tz.clone(),
        // Paths are masked already (`$HOME` as `~`).
        home: None,
        size: *size,
        view: *view,
        help: *help,
        accounts: accounts.iter().map(|a| r.account_state(a)).collect(),
        accounts_list: *accounts_list,
        checks: checks
            .as_ref()
            .map(|checks| checks.iter().map(|c| r.check(c)).collect()),
        checks_in_flight: *checks_in_flight,
        index: r.index(index),
        indexing: *indexing,
        index_in_flight: *index_in_flight,
        index_loaded: *index_loaded,
        index_refreshed: *index_refreshed,
        index_error: r.scrub_opt(index_error),
        by_session: by_session
            .iter()
            .map(|(id, p)| (id.clone(), key_path(p)))
            .collect::<HashMap<_, _>>(),
        attribution_base: attribution_base.redacted(|q| r.alias(q), key_path),
        attribution: attribution.redacted(|q| r.alias(q), key_path),
        attribution_in_flight: *attribution_in_flight,
        live: live.iter().map(|s| r.live_session(s)).collect(),
        live_rows: live_rows.clone(),
        live_show_inactive: *live_show_inactive,
        live_list: *live_list,
        logs: logs.as_ref().map(|l| r.logs(l)),
        live_loaded: *live_loaded,
        live_in_flight: *live_in_flight,
        live_stale: *live_stale,
        live_updated: *live_updated,
        history: r.history(history),
        preview: r.preview(preview),
        stats: r.stats(stats),
        notice: notice.as_ref().map(|n| r.notice(n)),
        overlay: overlay.as_ref().map(|o| r.overlay(o)),
        stores: stores
            .as_ref()
            .map(|stores| stores.iter().map(|s| r.store(s)).collect()),
        pending: pending
            .as_ref()
            .map(|(check, request)| (*check, r.request(request))),
        launch_checks: *launch_checks,
        cancelled: r.scrub_opt(cancelled),
        form_check: *form_check,
        index_again: *index_again,
        attribution_again: *attribution_again,
        live_again: *live_again,
        private: *private,
        // Its keys are the names private mode hides; nothing drawn needs them.
        aliases: Aliases::default(),
    }
}

/// [`redacted`], made again only when the app changed other than its clock. The copy holds the
/// whole index, so making it takes about 26 ms of CPU for 7.6K sessions: too slow for every
/// frame (every 100 ms tick), while comparing the app with the one it was made from takes
/// about 1 ms.
#[derive(Debug, Default)]
pub struct Snapshot {
    /// The app the copy was made from, and the copy.
    made: Option<(App, App)>,
}

impl Snapshot {
    pub fn of(&mut self, app: &App) -> &App {
        let current = match &mut self.made {
            Some((from, _)) => {
                from.now = app.now;
                *from == *app
            }
            None => false,
        };
        if !current {
            self.made = Some((app.clone(), redacted(app)));
        }
        let (_, copy) = self.made.as_mut().expect("made above");
        copy.now = app.now;
        copy
    }
}

/// What [`redacted`] masks with.
struct Redactor<'a> {
    scrub: Scrubber,
    aliases: &'a Aliases,
    home: Option<&'a str>,
}

impl Redactor<'_> {
    fn alias(&self, qualified: &str) -> String {
        self.aliases.qualified(qualified)
    }

    fn path(&self, p: &str) -> String {
        mask_path(p, self.home)
    }

    fn path_buf(&self, p: &Path) -> PathBuf {
        PathBuf::from(self.path(&p.to_string_lossy()))
    }

    fn scrub_opt(&self, text: &Option<String>) -> Option<String> {
        text.as_deref().map(|t| self.scrub.text(t))
    }

    fn scrub_result<T>(
        &self,
        result: &Result<T, String>,
        ok: impl FnOnce(&T) -> T,
    ) -> Result<T, String> {
        match result {
            Ok(v) => Ok(ok(v)),
            Err(e) => Err(self.scrub.text(e)),
        }
    }

    fn account(&self, account: &Account) -> Account {
        let Account {
            provider,
            name: _,
            home,
        } = account;
        Account {
            provider: *provider,
            name: self.aliases.name(&account.qualified()),
            home: match home {
                Home::Default => Home::Default,
                Home::Path(p) => Home::Path(self.path(p)),
            },
        }
    }

    fn account_state(&self, a: &AccountState) -> AccountState {
        let AccountState {
            account,
            identity,
            identity_pending,
            cached,
            cached_pending,
            live,
            live_pending,
        } = a;
        AccountState {
            account: self.account(account),
            identity: identity.as_ref().map(identity_redacted),
            identity_pending: *identity_pending,
            cached: cached.as_ref().map(|c| self.scrub_result(c, cached_usage)),
            cached_pending: *cached_pending,
            live: live.as_ref().map(|l| {
                self.scrub_result(l, |(rows, at)| (rows.iter().map(usage_row).collect(), *at))
            }),
            live_pending: *live_pending,
        }
    }

    fn check(&self, check: &Check) -> Check {
        let Check { account, message } = check;
        Check {
            account: account.as_deref().map(|q| self.alias(q)),
            message: self.scrub.text(message),
        }
    }

    fn index(&self, index: &Index) -> Index {
        let Index {
            schema_version,
            entries,
        } = index;
        Index {
            schema_version: *schema_version,
            entries: entries
                .iter()
                .map(|(p, e)| (key_path(p), self.entry(e)))
                .collect(),
        }
    }

    fn entry(&self, e: &Entry) -> Entry {
        let Entry {
            provider,
            session_id,
            path,
            store,
            size,
            mtime_ns,
            ino,
            scanned_offset,
            gap,
            title,
            first_user_text,
            cwd_first,
            cwd_last,
            ts_first,
            ts_last,
            entrypoint,
            source,
            originator,
        } = e;
        let masked = |t: &Option<String>| t.as_ref().map(|_| MASK.to_string());
        Entry {
            provider: *provider,
            session_id: session_id.clone(),
            path: key_path(path),
            store: key_path(store),
            size: *size,
            mtime_ns: *mtime_ns,
            ino: *ino,
            scanned_offset: *scanned_offset,
            gap: *gap,
            title: masked(title),
            first_user_text: masked(first_user_text),
            cwd_first: cwd_first.as_deref().map(|c| self.path(c)),
            cwd_last: cwd_last.as_deref().map(|c| self.path(c)),
            ts_first: ts_first.clone(),
            ts_last: ts_last.clone(),
            entrypoint: entrypoint.clone(),
            source: source.clone(),
            originator: originator.clone(),
        }
    }

    fn live_session(&self, s: &LiveSession) -> LiveSession {
        let LiveSession {
            account,
            pid,
            short_id,
            cwd,
            kind,
            started_at,
            session_id,
            name,
            status,
            source,
        } = s;
        LiveSession {
            account: self.alias(account),
            pid: *pid,
            short_id: short_id.clone(),
            cwd: cwd.as_deref().map(|c| self.path(c)),
            kind: kind.clone(),
            started_at: *started_at,
            session_id: session_id.clone(),
            name: name.as_ref().map(|_| MASK.to_string()),
            status: status.clone(),
            source: *source,
        }
    }

    fn logs(&self, logs: &Logs) -> Logs {
        let Logs {
            key: (account, id),
            short_id,
            result,
        } = logs;
        Logs {
            key: (self.alias(account), live_id(id)),
            short_id: short_id.clone(),
            result: result
                .as_ref()
                .map(|r| self.scrub_result(r, |_| MASK.to_string())),
        }
    }

    fn history(&self, history: &History) -> History {
        let History {
            rows,
            list,
            show_all,
            query,
            searching,
        } = history;
        History {
            rows: rows.iter().map(|p| key_path(p)).collect(),
            list: *list,
            show_all: *show_all,
            query: if query.is_empty() {
                String::new()
            } else {
                MASK.to_string()
            },
            searching: *searching,
        }
    }

    fn preview(&self, preview: &Preview) -> Preview {
        let Preview {
            target,
            settled,
            loading,
            loaded,
            expanded,
            scroll,
        } = preview;
        Preview {
            target: target.as_deref().map(key_path),
            settled: *settled,
            loading: loading.as_deref().map(key_path),
            loaded: loaded.as_ref().map(|(p, result)| {
                let messages = |m: &Vec<Message>| m.iter().map(message).collect();
                (key_path(p), self.scrub_result(result, messages))
            }),
            expanded: *expanded,
            scroll: *scroll,
        }
    }

    fn stats(&self, stats: &StatsState) -> StatsState {
        let StatsState {
            report,
            error,
            in_flight,
            requested,
            progress,
            computed,
            period,
            scroll,
        } = stats;
        StatsState {
            report: report.as_ref().map(|r| self.report(r)),
            error: self.scrub_opt(error),
            in_flight: *in_flight,
            requested: *requested,
            progress: *progress,
            computed: *computed,
            period: *period,
            scroll: *scroll,
        }
    }

    fn report(&self, report: &Report) -> Report {
        let Report { tables, files } = report;
        let tables = tables
            .iter()
            .map(|table| {
                let Table {
                    period,
                    since,
                    sections,
                    overall,
                    series,
                } = table;
                Table {
                    period: *period,
                    since: *since,
                    sections: sections
                        .iter()
                        .map(|s| {
                            let Section { accounts, models } = s;
                            Section {
                                accounts: accounts.iter().map(|q| self.alias(q)).collect(),
                                models: models.iter().map(model_row).collect(),
                            }
                        })
                        .collect(),
                    overall: overall.iter().map(model_row).collect(),
                    // Numbers over time; no account in it.
                    series: series.clone(),
                }
            })
            .collect();
        Report {
            tables,
            files: *files,
        }
    }

    fn notice(&self, notice: &Notice) -> Notice {
        let Notice { text, level } = notice;
        Notice {
            text: self.scrub.text(text),
            level: *level,
        }
    }

    fn overlay(&self, overlay: &Overlay) -> Overlay {
        match overlay {
            Overlay::Pick(pick) => {
                let Pick {
                    path,
                    session_id,
                    action,
                    options,
                    attributed,
                    selected,
                } = pick;
                Overlay::Pick(Pick {
                    path: key_path(path),
                    session_id: session_id.clone(),
                    action: *action,
                    options: options.iter().map(|q| self.alias(q)).collect(),
                    attributed: attributed.iter().map(|q| self.alias(q)).collect(),
                    selected: *selected,
                })
            }
            Overlay::Form(form) => {
                let Form {
                    kind,
                    fields,
                    focus,
                    error,
                } = form;
                Overlay::Form(Form {
                    kind: match kind {
                        FormKind::NewSession { account } => FormKind::NewSession {
                            account: self.account(account),
                        },
                        FormKind::Setup => FormKind::Setup,
                    },
                    fields: fields
                        .iter()
                        .map(|field| Field {
                            label: field.label,
                            value: masked_value(field, self.home),
                            mask: field.mask,
                        })
                        .collect(),
                    focus: *focus,
                    error: self.scrub_opt(error),
                })
            }
            Overlay::Confirm(confirm) => {
                let Confirm {
                    verb,
                    account,
                    short_id,
                } = confirm;
                Overlay::Confirm(Confirm {
                    verb: *verb,
                    account: self.alias(account),
                    short_id: short_id.clone(),
                })
            }
            Overlay::ResumeCodex(confirm) => {
                let ResumeCodex {
                    path,
                    written,
                    request,
                } = confirm;
                Overlay::ResumeCodex(ResumeCodex {
                    path: key_path(path),
                    written: *written,
                    request: self.request(request),
                })
            }
            Overlay::RemoveAccount(account) => Overlay::RemoveAccount(self.account(account)),
        }
    }

    fn store(&self, store: &Store) -> Store {
        let Store {
            provider,
            path,
            accounts,
            thread_names,
        } = store;
        Store {
            provider: *provider,
            path: key_path(path),
            accounts: accounts.iter().map(|q| self.alias(q)).collect(),
            thread_names: thread_names.iter().map(|p| key_path(p)).collect(),
        }
    }

    /// Arguments that are options or session IDs stay; any other (a name, a path) is masked.
    fn request(&self, request: &LaunchRequest) -> LaunchRequest {
        let LaunchRequest {
            account,
            args,
            cwd,
            what,
        } = request;
        LaunchRequest {
            account: self.account(account),
            args: args
                .iter()
                .map(|a| {
                    if a.starts_with('-') || launch::is_session_id(a) {
                        a.clone()
                    } else {
                        MASK.to_string()
                    }
                })
                .collect(),
            cwd: cwd.as_deref().map(|p| self.path_buf(p)),
            what: self.scrub.text(what),
        }
    }
}

/// The email as `•••@•••` and the organization as `•••`; the plan, login method and where it
/// was read stay.
fn identity_redacted(identity: &Identity) -> Identity {
    match identity {
        Identity::LoggedIn {
            email,
            org,
            plan,
            method,
            cached,
        } => Identity::LoggedIn {
            email: email.as_ref().map(|_| MASKED_EMAIL.to_string()),
            org: org.as_ref().map(|_| MASK.to_string()),
            plan: plan.clone(),
            method: method.clone(),
            cached: *cached,
        },
        Identity::NotLoggedIn => Identity::NotLoggedIn,
        Identity::Unknown => Identity::Unknown,
    }
}

/// Usage is numbers, limit labels (model names) and reset times: all shown.
fn cached_usage(usage: &CachedUsage) -> CachedUsage {
    let CachedUsage { fetched_at, rows } = usage;
    CachedUsage {
        fetched_at: *fetched_at,
        rows: rows.iter().map(usage_row).collect(),
    }
}

fn usage_row(row: &UsageRow) -> UsageRow {
    let UsageRow {
        label,
        percent,
        severity,
        resets,
    } = row;
    UsageRow {
        label: label.clone(),
        percent: *percent,
        severity: severity.clone(),
        resets: resets.as_ref().map(|r| match r {
            Resets::At(at) => Resets::At(*at),
            Resets::Text(text) => Resets::Text(text.clone()),
        }),
    }
}

/// Pids and session IDs stay.
fn live_id(id: &LiveId) -> LiveId {
    match id {
        LiveId::Pid(pid) => LiveId::Pid(*pid),
        LiveId::Short(short) => LiveId::Short(short.clone()),
        LiveId::Session(session) => LiveId::Session(session.clone()),
        LiveId::Unknown => LiveId::Unknown,
    }
}

fn message(m: &Message) -> Message {
    let Message { role, text: _ } = m;
    Message {
        role: *role,
        text: MASK.to_string(),
    }
}

/// Model names, counts and costs are shown.
fn model_row(m: &ModelRow) -> ModelRow {
    let ModelRow {
        provider,
        model,
        tokens,
        cost,
    } = m;
    ModelRow {
        provider: *provider,
        model: model.clone(),
        tokens: *tokens,
        cost: *cost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_keep_their_shape_only() {
        let home = Some("/Users/you");
        assert_eq!(mask_path("/Users/you/space/remuda", home), "~/•••/•••");
        assert_eq!(mask_path("/Users/you", home), "~");
        assert_eq!(mask_path("/Users/you/", home), "~");
        assert_eq!(mask_path("/Users/yours/x", home), "/•••/•••/•••");
        assert_eq!(mask_path("/tmp", home), "/•••");
        assert_eq!(mask_path("/", home), "/");
        assert_eq!(mask_path("//a//b/", None), "/•••/•••");
        assert_eq!(mask_path("~/work/x", None), "~/•••/•••");
        assert_eq!(mask_path("relative/dir", None), "•••/•••");
        assert_eq!(mask_path("", home), "");
        assert_eq!(
            mask_path("/Users/you/a b/c", Some("/Users/you/")),
            "~/•••/•••"
        );
    }

    #[test]
    fn key_paths_differ_and_hide_the_path() {
        let a = key_path(Path::new("/Users/you/.claude/projects/-w/a.jsonl"));
        let b = key_path(Path::new("/Users/you/.claude/projects/-w/b.jsonl"));
        assert_ne!(a, b);
        assert_eq!(
            a,
            key_path(Path::new("/Users/you/.claude/projects/-w/a.jsonl"))
        );
        assert!(!a.to_string_lossy().contains("you"), "{a:?}");
    }

    #[test]
    fn aliases_are_per_provider_stable_and_never_the_name() {
        let mut aliases = Aliases::default();
        for q in [
            "claude:default",
            "claude:max",
            "codex:work",
            "claude:team",
            "claude:max",
        ] {
            aliases.note(q);
        }
        assert_eq!(aliases.qualified("claude:default"), "claude:default");
        assert_eq!(aliases.qualified("claude:max"), "claude:account-1");
        assert_eq!(aliases.qualified("claude:team"), "claude:account-2");
        assert_eq!(aliases.qualified("codex:work"), "codex:account-1");
        assert_eq!(aliases.name("claude:team"), "account-2");
        assert_eq!(aliases.name("codex:work"), "account-1");
        assert_eq!(aliases.qualified("claude:gone"), "claude:account-?");
        assert_eq!(aliases.qualified("codex:default"), "codex:default");
        assert_eq!(aliases.qualified("secret"), "account-?");
        assert_eq!(aliases.qualified("secret:thing"), "account-?");
        // Grow-only: a later account takes the next number.
        aliases.note("claude:new");
        assert_eq!(aliases.qualified("claude:new"), "claude:account-3");
        assert_eq!(aliases.qualified("claude:max"), "claude:account-1");
    }

    fn scrubber(names: &[&str], secrets: &[(&str, &str)]) -> Scrubber {
        let mut aliases = Aliases::default();
        for n in names {
            aliases.note(n);
        }
        let mut s = Scrubber {
            home: Some("/Users/you".into()),
            secrets: secrets
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            paths: vec!["/Users/you".into()],
            names: Vec::new(),
        };
        let mut qualified: Vec<(String, String)> = Vec::new();
        let mut bare: Vec<(String, String)> = Vec::new();
        for (name, alias) in aliases.pairs() {
            let (_, short) = name.split_once(':').unwrap();
            if short == DEFAULT_NAME {
                continue;
            }
            qualified.push((name.into(), alias.into()));
            bare.push((short.into(), alias.split_once(':').unwrap().1.into()));
        }
        qualified.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
        bare.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
        qualified.extend(bare);
        s.names = qualified;
        s
    }

    #[test]
    fn scrubber_masks_quotes_secrets_paths_emails_and_names() {
        let s = scrubber(
            &["claude:default", "claude:max", "claude:maxi", "codex:work"],
            &[("Acme Corp", MASK), ("fix index", MASK)],
        );
        // Quoted text.
        assert_eq!(
            s.text("new session “my secret” as max"),
            "new session “•••” as account-1"
        );
        // Exact secrets.
        assert_eq!(s.text("org Acme Corp: fix index"), "org •••: •••");
        // Paths, up to `: `, `, `, a quote or a bracket.
        assert_eq!(
            s.text("/Users/you/a b/c does not exist: max"),
            "~/•••/•••: account-1"
        );
        assert_eq!(
            s.text("cannot use (/tmp/x), \"~/y/z\" or [/a/b]; x=/c"),
            "cannot use (/•••/•••), \"~/•••/•••\" or [/•••/•••]; x=/•••"
        );
        // A `/` inside a word is not a path.
        assert_eq!(s.text("claude/codex and and/or"), "claude/codex and and/or");
        // Emails.
        assert_eq!(
            s.text("logged in as (me@x.com) ok"),
            "logged in as •••@••• ok"
        );
        // Names, qualified first, whole words only, once: an alias is not replaced again.
        assert_eq!(
            s.text("max, maxi, claude:max, codex:work, work: maximum claude:default default"),
            "account-1, account-2, claude:account-1, codex:account-1, account-1: maximum \
             claude:default default"
        );
    }

    /// A known path starts a path anywhere, but only as whole components: `$HOME` is not a
    /// prefix of a sibling (`/Users/yours`), and a trailing `/` in it changes nothing.
    #[test]
    fn scrubber_masks_known_paths_at_component_boundaries() {
        let mut s = scrubber(&["claude:max"], &[]);
        assert_eq!(
            s.text("home:/Users/you/secret/x: gone"),
            "home:~/•••/•••: gone"
        );
        assert_eq!(s.text("key=/Users/you"), "key=~");
        assert_eq!(s.text("see /Users/yours/secret"), "see /•••/•••/•••");
        assert_eq!(s.text("x:/Users/yours/secret"), "x:/Users/yours/secret");
        s.home = Some("/Users/you/".into());
        assert_eq!(s.text("see /Users/you/other/x"), "see ~/•••/•••");
        s.home = Some("/Users/yo".into());
        s.paths = vec!["/Users/yo".into()];
        assert_eq!(s.text("see /Users/you/proj"), "see /•••/•••/•••");
        assert_eq!(s.text("x:/Users/you/proj"), "x:/Users/you/proj");
    }

    #[test]
    fn scrubber_leaves_plain_text() {
        let s = scrubber(&["claude:max"], &[]);
        for text in [
            "claude exited 0",
            "resume 766560c5 as default",
            "34% · resets in 2h",
            "",
        ] {
            assert_eq!(s.text(text), text);
        }
    }
}
