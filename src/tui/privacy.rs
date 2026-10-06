//! Private mode (SPEC R21): what the TUI shows while it is on. [`redacted`] makes a copy of
//! the [`App`] with account names aliased and emails, organizations, paths, session content
//! and the descriptions of the configuration pane (R22) masked, and the screen is drawn from
//! that copy only. Every struct and enum it
//! copies is destructured without `..`: a field added later does not compile until it is
//! decided how private mode shows it.

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::account_config::{
    ConfigView, Content, Entry as ConfigEntry, Item, Mcp, Memory, Origin, Plugin, PluginContents,
    Role as ConfigRole, Summary, Synced,
};
use crate::checks::Check;
use crate::identity::Identity;
use crate::index::{Entry, Index, Store};
use crate::launch;
use crate::live::{LiveId, LiveSession};
use crate::privacy::{Aliases, Piece, alias_words};
use crate::registry::{Account, DEFAULT_NAME, Home};
use crate::share::Skip;
use crate::stats::{self, ModelRow, Report, Section, Table};
use crate::transcript::Message;
use crate::usage::{CachedUsage, Resets, UsageRow};

use super::app::{
    AccountState, App, Background, ConfigPane, Confirm, Field, Form, FormKind, History,
    LaunchRequest, Logs, Marked, Mask, Notice, Overlay, Pick, Preview, ResumeCodex, StatsState,
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

/// Masks messages and free text (notices, errors, check messages, a launch's description;
/// R21), knowing the app's names and secrets. A [`Marked`] message says what each of its pieces
/// is, so a path in it is masked as the path it is; only text from elsewhere is searched for
/// paths ([`Scrubber::text`]).
pub struct Scrubber {
    home: Option<String>,
    /// Exact text and what it becomes, longest first.
    secrets: Vec<(String, String)>,
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
            if text.trim().chars().count() >= MIN_SECRET {
                secrets.push((text.to_string(), shown));
            }
        };
        for a in &app.accounts {
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
        secret(&app.history.query, MASK.to_string());
        if let Some(Overlay::Form(form)) = &app.overlay {
            for field in &form.fields {
                if field.mask == Mask::Plain {
                    continue;
                }
                let mut shown = masked_value(field, home.as_deref());
                if field.mask == Mask::Path {
                    // As the form reads the directory, not with the blanks typed around it.
                    shown = mask_path(field.value.trim(), home.as_deref());
                }
                // As typed, and as the form reads it (without the blanks around it); each also
                // as an error quotes it (`{:?}`).
                for value in [field.value.as_str(), field.value.trim()] {
                    secret(value, shown.clone());
                    let quoted = format!("{value:?}");
                    secret(&quoted[1..quoted.len() - 1], shown.clone());
                }
            }
        }
        secrets.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.cmp(b)));
        secrets.dedup();

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
            names: qualified,
        }
    }

    /// Text from elsewhere, or of unknown origin: `text` with, in order: `“…”` masked; the
    /// app's secrets (emails, organizations, live session names, typed values, the search)
    /// masked; each line masked from its first path on; each word with an `@` masked; account
    /// names replaced by their aliases, as whole words.
    pub fn text(&self, text: &str) -> String {
        self.scrub(text, true)
    }

    /// Words remuda wrote itself ([`Piece::Words`]): as [`Scrubber::text`], except that no
    /// path is looked for. There is none in them: a message says each of its paths as a piece
    /// of its own, so a `/login` or an `and/or` in remuda's words stays as written.
    pub fn words(&self, words: &str) -> String {
        self.scrub(words, false)
    }

    fn scrub(&self, text: &str, paths: bool) -> String {
        let mut out = mask_quoted(text);
        for (secret, shown) in &self.secrets {
            out = out.replace(secret.as_str(), shown);
        }
        if paths {
            out = self.mask_paths(&out);
        }
        let out = mask_emails(&out);
        alias_words(&out, &self.names)
    }

    /// `marked` piece by piece, each on its own: a path whole, as a path; remuda's words as
    /// [`Scrubber::words`]; text from elsewhere as [`Scrubber::text`], so what must be guessed
    /// there never reaches into the next piece.
    pub fn marked(&self, marked: &Marked) -> Marked {
        let mut out = Marked::default();
        for (piece, kind) in marked.pieces() {
            out = match kind {
                Piece::Path => out.path(mask_path(piece, self.home.as_deref())),
                Piece::Words => out.words(self.words(piece)),
                Piece::Text => out.text(self.text(piece)),
            };
        }
        out
    }

    /// The fallback for text remuda did not write: each line from its first path to its end,
    /// as one path. Where a path ends cannot be told (one may hold blanks, `: `, `, `, quotes
    /// and brackets), so the rest of the line is taken for it: that may hide what follows a
    /// path, never show a part of one. A path starts where [`path_start`] finds one.
    fn mask_paths(&self, text: &str) -> String {
        let mut out = String::new();
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let start = path_start(line).unwrap_or(line.len());
            out.push_str(&line[..start]);
            out.push_str(&mask_path(&line[start..], self.home.as_deref()));
        }
        out
    }
}

/// Where the first path of `line` starts: in its first word that holds a `/` (`/a/b`, `/a`,
/// `~/a`, `./a/b`, `a/b`). At the `/` or `~/` itself when it begins the word or follows a
/// quote, a bracket, `=` or `:` in it (`home:/Users/you` keeps `home:`); otherwise the word is
/// a relative path, from its start or from after the last quote, bracket or `=` before the
/// `/`. Never after the line's first `/`: every `/name` is a path here, one that reads like a
/// slash command of Claude too (remuda's own messages name those in their words, which are not
/// searched).
fn path_start(line: &str) -> Option<usize> {
    const OPENERS: [char; 7] = ['(', '[', '"', '\'', '“', '`', '='];
    let slash = line.find('/')?;
    let word = line[..slash]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let before = &line[word..slash];
    let (before, root) = match before.strip_suffix('~') {
        Some(before) => (before, slash - 1),
        None => (before, slash),
    };
    if before
        .chars()
        .next_back()
        .is_some_and(|c| c != ':' && !OPENERS.contains(&c))
    {
        let opener = before
            .char_indices()
            .rev()
            .find(|(_, c)| OPENERS.contains(c))
            .map_or(0, |(i, c)| i + c.len_utf8());
        return Some(word + opener);
    }
    Some(root)
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
        config,
        index,
        indexing,
        index_loaded,
        index_refreshed,
        index_error,
        by_session,
        attribution_base,
        attribution,
        live,
        live_rows,
        live_show_inactive,
        live_list,
        logs,
        live_loaded,
        live_updated,
        history,
        preview,
        stats,
        notice,
        overlay,
        stores,
        work,
        cancelled,
        form_check,
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
        config: r.config_pane(config),
        index: r.index(index),
        indexing: *indexing,
        index_loaded: *index_loaded,
        index_refreshed: *index_refreshed,
        index_error: index_error.as_ref().map(|e| r.scrub.marked(e)),
        by_session: by_session
            .iter()
            .map(|(id, p)| (id.clone(), key_path(p)))
            .collect::<HashMap<_, _>>(),
        attribution_base: attribution_base.redacted(|q| r.alias(q)),
        attribution: attribution.redacted(|q| r.alias(q)),
        live: live.iter().map(|s| r.live_session(s)).collect(),
        live_rows: live_rows.clone(),
        live_show_inactive: *live_show_inactive,
        live_list: *live_list,
        logs: logs.as_ref().map(|l| r.logs(l)),
        live_loaded: *live_loaded,
        live_updated: *live_updated,
        history: r.history(history),
        preview: r.preview(preview),
        stats: r.stats(stats),
        notice: notice.as_ref().map(|n| r.notice(n)),
        overlay: overlay.as_ref().map(|o| r.overlay(o)),
        stores: stores
            .as_ref()
            .map(|stores| stores.iter().map(|s| r.store(s)).collect()),
        work: r.background(work),
        cancelled: r.scrub_opt(cancelled),
        form_check: *form_check,
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
            cached,
            live,
            work,
        } = a;
        AccountState {
            account: self.account(account),
            identity: identity.as_ref().map(identity_redacted),
            cached: cached.as_ref().map(|c| self.scrub_result(c, cached_usage)),
            live: live.as_ref().map(|l| {
                self.scrub_result(l, |(rows, at)| (rows.iter().map(usage_row).collect(), *at))
            }),
            // What is out, not what it found.
            work: work.clone(),
        }
    }

    /// What is out: the launch being checked is shown in the status line, so it is masked like
    /// any other, and so is the account whose logs are read; a short id is shown as it is, like
    /// [`Logs::short_id`].
    fn background(&self, work: &Background) -> Background {
        let Background {
            index,
            live,
            attribution,
            checks,
            stats,
            launch,
            logs,
        } = work;
        Background {
            index: index.clone(),
            live: live.clone(),
            attribution: attribution.clone(),
            checks: checks.clone(),
            stats: stats.clone(),
            launch: launch.map(|request| self.request(request)),
            logs: logs.map(|(account, short_id)| (self.account(account), short_id.clone())),
        }
    }

    fn check(&self, check: &Check) -> Check {
        let Check { account, message } = check;
        Check {
            account: account.as_deref().map(|q| self.alias(q)),
            message: self.scrub.marked(message),
        }
    }

    fn index(&self, index: &Index) -> Index {
        // What the cache remembers of each account's store (names and real paths) is the
        // refresh's and is never drawn: the copy holds none of it.
        let Index {
            schema_version,
            entries,
            stores: _,
        } = index;
        Index {
            schema_version: *schema_version,
            entries: entries
                .iter()
                .map(|(p, e)| (key_path(p), self.entry(e)))
                .collect(),
            stores: Default::default(),
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
            work,
            loaded,
            expanded,
            scroll,
        } = preview;
        Preview {
            target: target.as_deref().map(key_path),
            settled: *settled,
            work: work.map(|p| key_path(p)),
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
            requested,
            progress,
            computed,
            period,
            scroll,
        } = stats;
        StatsState {
            report: report.as_ref().map(|r| self.report(r)),
            error: error.as_ref().map(|e| self.scrub.marked(e)),
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
            text: self.scrub.marked(text),
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
                    error: error.as_ref().map(|e| self.scrub.marked(e)),
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

    /// The Configuration pane (R22): names stay, descriptions are masked, paths too.
    fn config_pane(&self, pane: &ConfigPane) -> ConfigPane {
        let ConfigPane {
            open,
            expanded,
            scroll,
            account,
            loaded,
            work,
        } = pane;
        ConfigPane {
            open: *open,
            expanded: *expanded,
            scroll: *scroll,
            account: account.as_ref().map(|a| self.account(a)),
            loaded: loaded
                .as_ref()
                .map(|l| self.scrub_result(l, |v| self.config_view(v))),
            work: work.map(|a| self.account(a)),
        }
    }

    fn config_view(&self, view: &ConfigView) -> ConfigView {
        let ConfigView {
            role,
            instructions,
            synced,
            stale_overrides,
            plugins,
            disabled_plugins,
            settings_origin,
            own_settings,
            shared_settings,
            withheld,
            memory,
            agent_memory,
            mcp,
            problems,
        } = view;
        let redact = |memory: &Memory| {
            let Memory { dir, origin, files } = memory;
            Memory {
                dir: dir.as_deref().map(|d| self.path(d)),
                origin: origin_copy(origin),
                files: *files,
            }
        };
        ConfigView {
            role: match role {
                ConfigRole::Alone => ConfigRole::Alone,
                ConfigRole::Source => ConfigRole::Source,
                ConfigRole::OptedOut { source } => ConfigRole::OptedOut {
                    source: self.alias(source),
                },
                ConfigRole::SourceMissing { source } => ConfigRole::SourceMissing {
                    source: self.alias(source),
                },
                ConfigRole::Member { source } => ConfigRole::Member {
                    source: self.alias(source),
                },
            },
            instructions: instructions.iter().map(|i| self.config_item(i)).collect(),
            synced: match synced {
                Synced::None => Synced::None,
                Synced::Skills { skills, others } => Synced::Skills {
                    skills: skills.iter().map(|e| self.config_entry(e)).collect(),
                    others: *others,
                },
                Synced::Unmatched { buckets } => Synced::Unmatched { buckets: *buckets },
            },
            stale_overrides: stale_overrides.clone(),
            plugins: plugins.iter().map(|p| self.plugin(p)).collect(),
            disabled_plugins: *disabled_plugins,
            settings_origin: origin_copy(settings_origin),
            own_settings: summary(own_settings),
            shared_settings: summary(shared_settings),
            withheld: withheld.clone(),
            memory: redact(memory),
            agent_memory: redact(agent_memory),
            mcp: {
                let Mcp { user, project } = mcp;
                Mcp {
                    user: user.clone(),
                    project: project.clone(),
                }
            },
            problems: problems.iter().map(|p| self.scrub.marked(p)).collect(),
        }
    }

    fn config_item(&self, item: &Item) -> Item {
        let Item {
            name,
            origin,
            link,
            content,
        } = item;
        Item {
            name,
            origin: origin_copy(origin),
            link: link.as_deref().map(|l| self.path_buf(l)),
            content: match content {
                Content::File { bytes, lines } => Content::File {
                    bytes: *bytes,
                    lines: *lines,
                },
                Content::Entries(entries) => {
                    Content::Entries(entries.iter().map(|e| self.config_entry(e)).collect())
                }
                Content::Broken => Content::Broken,
            },
        }
    }

    /// Names, models, efforts and tools stay (R21); the description is masked.
    fn config_entry(&self, entry: &ConfigEntry) -> ConfigEntry {
        let ConfigEntry {
            name,
            description,
            model,
            effort,
            tools,
            link,
            broken,
            off,
        } = entry;
        ConfigEntry {
            name: name.clone(),
            description: description.as_ref().map(|_| MASK.to_string()),
            model: model.clone(),
            effort: effort.clone(),
            tools: tools.clone(),
            link: link.as_deref().map(|l| self.path_buf(l)),
            broken: *broken,
            off: *off,
        }
    }

    fn plugin(&self, plugin: &Plugin) -> Plugin {
        let Plugin {
            name,
            origin,
            version,
            scope,
            installs,
            path,
            contents,
        } = plugin;
        Plugin {
            name: name.clone(),
            origin: origin_copy(origin),
            version: version.clone(),
            scope: scope.clone(),
            installs: *installs,
            path: path.as_deref().map(|p| self.path_buf(p)),
            contents: contents.as_ref().map(|c| {
                let PluginContents {
                    agents,
                    skills,
                    commands,
                    hooks,
                    mcp_servers,
                } = c;
                PluginContents {
                    agents: agents.iter().map(|e| self.config_entry(e)).collect(),
                    skills: skills.iter().map(|e| self.config_entry(e)).collect(),
                    commands: commands.iter().map(|e| self.config_entry(e)).collect(),
                    hooks: hooks.clone(),
                    mcp_servers: mcp_servers.clone(),
                }
            }),
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

fn origin_copy(origin: &Origin) -> Origin {
    match origin {
        Origin::Own => Origin::Own,
        Origin::Shared => Origin::Shared,
        Origin::AlreadySource => Origin::AlreadySource,
        Origin::NotShared(skip) => Origin::NotShared(match skip {
            Skip::TurnedOff => Skip::TurnedOff,
            Skip::OwnInstall => Skip::OwnInstall,
            Skip::NotInstalled => Skip::NotInstalled,
            Skip::UnrecognizedList => Skip::UnrecognizedList,
        }),
        Origin::Project => Origin::Project,
    }
}

/// Key names and counts only: shown as they are (R21, R22).
fn summary(summary: &Summary) -> Summary {
    let Summary {
        model,
        permissions,
        hooks,
        env,
        status_line,
        other,
    } = summary;
    Summary {
        model: model.clone(),
        permissions: *permissions,
        hooks: hooks.clone(),
        env: env.clone(),
        status_line: *status_line,
        other: other.clone(),
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
        // A path, and with it the rest of its line.
        assert_eq!(s.text("max: /Users/you/a b/c"), "account-1: ~/•••/•••");
        assert_eq!(s.text("/Users/you/a b/c does not exist: max"), "~/•••/•••");
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
        // A name ends where an ASCII letter, a digit, `_` or `-` does not follow.
        assert_eq!(
            s.text("把max留给maxi（work）"),
            "把account-1留给account-2（account-1）"
        );
    }

    /// R21: where a path ends in text remuda did not write cannot be told, so the line is
    /// masked from where its first path starts: nothing of a path with blanks, brackets,
    /// quotes, `, ` or `: ` in it shows, and neither does a relative one.
    #[test]
    fn scrubber_masks_a_line_from_its_first_path() {
        let s = scrubber(&["claude:max"], &[]);
        for (text, masked) in [
            (
                "/Users/you/Dropbox (Personal)/clients/acme does not exist",
                "~/•••/•••/•••",
            ),
            (
                "/Volumes/Bob's Disk/secret-client/repo does not exist",
                "/•••/•••/•••/•••",
            ),
            (
                "max cannot find session 766560c5: its projects store is /Users/you/a, b/projects, \
                 the transcript is in /Users/you/x/projects",
                "account-1 cannot find session 766560c5: its projects store is \
                 ~/•••/•••/•••/•••/•••/•••",
            ),
            (
                "cannot open /srv/a: b/c; d/e \"f\"/g [h]/i: permission denied",
                "cannot open /•••/•••/•••/•••/•••/•••",
            ),
            (
                "cannot use (/tmp/x), \"~/y/z\" or [/a/b]; x=/c",
                "cannot use (/•••/•••/•••/•••/•••/•••/•••",
            ),
            // Relative paths: any word with a `/`.
            (
                "cannot read ./relative/secret/dir: permission denied",
                "cannot read •••/•••/•••/•••",
            ),
            ("see a/b/c for details", "see •••/•••/•••"),
            ("claude/codex and and/or", "•••/•••/•••"),
            ("at home~/x y", "at •••/•••"),
            // What is before the path in its word stays when it ends with a quote, a bracket,
            // `=` or `:`; a relative path starts after the last quote, bracket or `=`.
            ("home:/Users/you/secret/x: gone", "home:~/•••/•••"),
            ("x:/Users/yours/secret", "x:/•••/•••/•••"),
            ("key=/Users/you", "key=~"),
            ("key=rel/dir (x)", "key=•••/•••"),
            ("in (./a/b) or c", "in (•••/•••/•••"),
            ("say \"~/y z\" ok", "say \"~/•••"),
            ("say “~/y z” ok", "say “•••” ok"),
            // Only to the end of the line.
            (
                "cannot run /x/y: no\nexit 1 for max\n\nin ~/z",
                "cannot run /•••/•••\nexit 1 for account-1\n\nin ~/•••",
            ),
        ] {
            assert_eq!(s.text(text), masked, "{text}");
        }
    }

    /// R21: in text remuda did not write, every `/name` is a path, masked with the rest of its
    /// line: one that reads like a slash command of Claude too.
    #[test]
    fn scrubber_takes_every_slash_name_in_text_from_elsewhere_for_a_path() {
        let s = scrubber(&["claude:max"], &[]);
        for (text, masked) in [
            ("see /loginx for more", "see /•••"),
            ("see /login-secret for more", "see /•••"),
            ("see /login_secret for more", "see /•••"),
            ("see /login.secret for more", "see /•••"),
            ("see /login/secret for more", "see /•••/•••"),
            ("see /login~ for more", "see /•••"),
            ("see /login，秘密 for more", "see /•••"),
            ("see ~/login for more", "see ~/•••"),
            ("see x/login for more", "see •••/•••"),
            ("see /Login for more", "see /•••"),
            ("see /memory and /usage", "see /•••/•••"),
            ("see /tmp or \"/var\" (x)", "see /•••/•••"),
            ("/ is the root", "/•••"),
            // The first component of a path with a blank in it.
            ("no /My Disk/secret here", "no /•••/•••"),
            // A directory may have a command's name: `/login` and `/rewind` are paths too.
            ("/login", "/•••"),
            ("run /login again as max", "run /•••"),
            ("see (/rewind) for max", "see (/•••"),
            ("/login then /a/b: c", "/•••/•••/•••"),
            (
                "/rewind finds nothing for max; link it too (ln -s /Users/you/a b/c d)",
                "/•••/•••/•••/•••/•••",
            ),
        ] {
            assert_eq!(s.text(text), masked, "{text}");
        }
    }

    /// R21: remuda's own words are not searched for paths (a message says each of its paths as
    /// a piece of its own), so a slash command or an `and/or` in them stays, and so does what
    /// the message says after a path. Quotes, secrets, emails and names are masked in them as
    /// anywhere.
    #[test]
    fn scrubber_looks_for_no_path_in_remudas_words() {
        let s = scrubber(&["claude:max"], &[("Acme Corp", MASK)]);
        for words in [
            "ANTHROPIC_API_KEY is set: it overrides every account's /login",
            "not file-history: /rewind does not find the file backups; link it too",
            "see (/login), \"/rewind\", `/login`, [/rewind]. x=/login; do:/rewind! as max",
            "teammate/sdk and/or max",
        ] {
            assert_eq!(s.words(words), words.replace("max", "account-1"), "{words}");
            let said = Marked::default().words(words);
            assert_eq!(s.marked(&said).as_str(), s.words(words), "{words}");
            // The same words, when it is not known who wrote them, hide their line from the
            // first `/` on.
            assert_ne!(s.text(words), s.words(words), "{words}");
        }
        let mixed = "“x” of Acme Corp, me@x.com and/or max: see /login";
        assert_eq!(
            s.words(mixed),
            "“•••” of •••, •••@••• and/or account-1: see /login"
        );
        assert_eq!(s.text(mixed), "“•••” of •••, •••@••• •••/•••/•••");
        // A check of R11: each path whole, and what is said between and after them stays.
        let said = Marked::default()
            .words(
                "shares projects with max through a symlink but not file-history: /rewind does \
                 not find the file backups of a session resumed from another account; link it \
                 too (",
            )
            .path("/Users/you/a, b (old)/file-history")
            .words(" -> ")
            .path("/srv/Bob's \"x\": y/file-history")
            .words(")");
        let masked = s.marked(&said);
        assert_eq!(
            masked.as_str(),
            "shares projects with account-1 through a symlink but not file-history: /rewind \
             does not find the file backups of a session resumed from another account; link \
             it too (~/•••/••• -> /•••/•••/•••)"
        );
        // Each piece is what it was.
        let kinds = |m: &Marked| m.pieces().map(|(_, kind)| kind).collect::<Vec<_>>();
        assert_eq!(kinds(&masked), kinds(&said));
        // A name read from a file is text from elsewhere: masked by itself, as far as its
        // piece goes.
        let said = Marked::default()
            .words("enabled plugin ")
            .text("tools/x@market")
            .words(" has no user install whose path exists: it is not shared");
        assert_eq!(
            s.marked(&said).as_str(),
            "enabled plugin •••/••• has no user install whose path exists: it is not shared"
        );
    }

    /// R21: an error keeps the reason remuda wrote after a path at any depth of its chain: a
    /// message that is the cause of another message is masked piece by piece like the outer
    /// one, where a chain nobody marked hides its line from the first path on.
    #[test]
    fn scrubber_masks_the_messages_in_an_error_piece_by_piece() {
        let s = scrubber(&["claude:max"], &[]);
        let file = "/Users/you/a, b (old)/settings.json";
        let inner = || -> anyhow::Error {
            Marked::default()
                .path(file)
                .words(" is not a JSON object")
                .into()
        };
        let private = |e: &anyhow::Error| s.marked(&Marked::from_error(e)).as_str().to_string();
        // The cause of a message of remuda.
        let outer = Marked::default()
            .words("cannot plan for max")
            .because(inner());
        assert_eq!(
            format!("{outer:#}"),
            format!("cannot plan for max: {file} is not a JSON object")
        );
        assert_eq!(
            private(&outer),
            "cannot plan for account-1: ~/•••/••• is not a JSON object"
        );
        // Under a context nobody marked.
        assert_eq!(
            private(&inner().context("cannot plan for max")),
            "cannot plan for account-1: ~/•••/••• is not a JSON object"
        );
        // Two levels down, the system's reason after it.
        let io = std::io::Error::other("Permission denied (os error 13)");
        let read = Marked::default()
            .words("cannot read ")
            .path(file)
            .because(io);
        let deep = Marked::default().words("cannot plan for max").because(
            Marked::default()
                .words("the settings of max are not usable")
                .because(read),
        );
        assert_eq!(
            private(&deep),
            "cannot plan for account-1: the settings of account-1 are not usable: cannot read \
             ~/•••/•••: Permission denied (os error 13)"
        );
        // The same error told by strings alone: its line is hidden from the first path on.
        let plain = anyhow::anyhow!("Permission denied (os error 13)")
            .context(format!("cannot read {file}"))
            .context("cannot plan for max");
        assert_eq!(
            private(&plain),
            "cannot plan for account-1: cannot read ~/•••/•••"
        );
    }

    /// R21: a path the app knows needs no rule of its own in text from elsewhere: it starts
    /// with a `/`, where a path starts anyway. One of a single component, one in the middle of
    /// a word, one that reads like a command.
    #[test]
    fn scrubber_masks_the_paths_the_app_knows_like_any_other() {
        let mut s = scrubber(&["claude:max"], &[]);
        s.home = Some("/privatehome".into());
        for (text, masked) in [
            (
                "cannot read /privatehome: permission denied",
                "cannot read /•••",
            ),
            ("cannot read /privatehome", "cannot read ~"),
            ("cannot read /privatehome/x: denied", "cannot read ~/•••"),
            ("in `/privatehome`.", "in `/•••"),
            ("at home:/privatehome, as max", "at home:/•••"),
            // Anywhere in a word.
            ("at x/privatehome/y", "at •••/•••/•••"),
            // Named like a command.
            ("run /login again", "run /•••"),
            ("run `/login`. again", "run `/•••"),
            ("in /rewind stuff/x y", "in /•••/•••"),
            ("in `/rewind stuff`. ok", "in `/•••"),
            ("in /rewind stuff. ok", "in /•••"),
            ("run /rewind again", "run /•••"),
            // A sibling of `$HOME` is not `~`, and is a path all the same.
            ("in /privatehomes", "in /•••"),
        ] {
            assert_eq!(s.text(text), masked, "{text}");
        }
    }

    /// `$HOME` shows as `~` only as whole components, and a trailing `/` in it changes nothing.
    #[test]
    fn scrubber_shows_home_only_at_component_boundaries() {
        let mut s = scrubber(&["claude:max"], &[]);
        assert_eq!(s.text("see /Users/yours/secret"), "see /•••/•••/•••");
        s.home = Some("/Users/you/".into());
        assert_eq!(s.text("see /Users/you/other/x"), "see ~/•••/•••");
        s.home = Some("/Users/yo".into());
        assert_eq!(s.text("see /Users/you/proj"), "see /•••/•••/•••");
        assert_eq!(s.text("x:/Users/you/proj"), "x:/•••/•••/•••");
    }

    /// R21: text remuda put together is masked piece by piece: a path whole, as a path,
    /// whatever characters it holds, and what follows it stays; a message from elsewhere
    /// hides its own line's end only.
    #[test]
    fn scrubber_masks_marked_text_piece_by_piece() {
        let s = scrubber(&["claude:max"], &[]);
        let said = Marked::from("max cannot find session 766560c5: its projects store is ")
            .path("/Users/you/a, b/projects")
            .text(", the transcript is in ")
            .path("/Users/you/Dropbox (Personal)/Bob's \"x\": y; z/projects")
            .text("; link ")
            .path("relative dir/projects")
            .text(" to that store to share sessions");
        let masked = s.marked(&said);
        assert_eq!(
            masked.as_str(),
            "account-1 cannot find session 766560c5: its projects store is ~/•••/•••, the \
             transcript is in ~/•••/•••/•••; link •••/••• to that store to share sessions"
        );
        assert_eq!(
            masked
                .pieces()
                .filter(|(_, kind)| *kind == Piece::Path)
                .count(),
            3
        );
        // A message from elsewhere is a piece of its own.
        let said = Marked::from("new session as max: ")
            .text("cannot run claude: /opt/secret bin/claude: no such file")
            .text(" · ")
            .text("warning: max reads 2 settings");
        assert_eq!(
            s.marked(&said).as_str(),
            "new session as account-1: cannot run claude: /•••/•••/••• · warning: account-1 \
             reads 2 settings"
        );
        assert_eq!(s.marked(&Marked::default()), Marked::default());
        assert_eq!(
            Marked::from("a")
                .text("")
                .path("")
                .text("b")
                .pieces()
                .count(),
            2
        );
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
