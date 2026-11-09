//! The one owner of the history files.
//!
//! Everything the bridge remembers between restarts that is *not* the directives lives in
//! `{dir}/{YYYY-MM-DD}.jsonl`, one Prague day per file. The ledger is the first producer, and
//! keeping the file handling in one actor means the writer, the reader, the rotation and the
//! retention policy cannot disagree with each other (board-0os).
//!
//! # Why a buffer at all
//!
//! A ledger row is ~250 B and there are 96 of them per day — ~25 KB/day. The write volume is
//! not the reason; the buffering exists so that **durability is a policy, not an accident**:
//! one `write_all` + one `sync_data` per flush instead of a syscall per row, a torn tail that
//! the reader can recognise, and a single place to change how often the card is written.
//! The real SD-card writer on this Pi is the persistent journald, not this file.
//!
//! # Crash and shutdown
//!
//! A buffer flush at most [`FLUSH_EVERY`] old is lost on SIGKILL or power cut; `main` asks for
//! a flush on the graceful shutdown path, which is what a deploy or a `systemctl restart`
//! takes. A hard crash can leave a partial last line, so [`StoreActor::read_range`] drops a
//! trailing line that is not newline-terminated — a reader must never see half a row.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use jiff::{
    Timestamp,
    civil::Date,
    tz::TimeZone,
};
use kameo::{Actor, message::Message};

/// Rows are held in memory for this long before hitting the card. Chosen for the write
/// count, not for the volume: at 96 rows/day it costs 48 appends of ~500 B.
pub const FLUSH_EVERY: Duration = Duration::from_secs(30 * 60);

/// A batch larger than this is flushed immediately, so a backfill cannot balloon the actor.
pub const MAX_BATCH: usize = 64 * 1024;

/// Day files older than this many days are removed at startup and on a day rollover.
pub const RETENTION_DAYS: i64 = 60;

/// The longest window a single read may cover, mirroring the view layer's 60-day cap. A
/// request for more is clamped, never rejected: the caller echoes the window it got.
pub const MAX_RANGE_DAYS: i64 = 62;

/// Where the files live and how long they are kept.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub dir: PathBuf,
    pub retention_days: i64,
    pub zone: TimeZone,
}

impl StoreConfig {
    /// `dir` plus the Prague zone and the default retention.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, jiff::Error> {
        Ok(Self {
            dir: dir.into(),
            retention_days: RETENTION_DAYS,
            zone: TimeZone::get("Europe/Prague")?,
        })
    }
}

/// Append one row. `at` is the row's own timestamp: it decides the day file, so a row is
/// never filed under the day it happened to be written on.
pub struct AppendRow {
    pub at: i64,
    pub line: String,
}

/// Write the batch to the card now.
pub struct Flush;

/// Read every row in `[from, to]`, day files in order. The pending batch is flushed first so
/// a query never silently misses the rows that were just appended.
pub struct ReadRange {
    pub from: i64,
    pub to: i64,
}

#[derive(Actor)]
pub struct StoreActor {
    cfg: StoreConfig,
    batch: String,
    batch_day: Option<String>,
}

impl StoreActor {
    /// Start the actor and sweep expired day files once.
    pub fn new(cfg: StoreConfig) -> Self {
        if let Err(error) = fs::create_dir_all(&cfg.dir) {
            // Not fatal on its own: every append and read reports its own failure, and a
            // bridge that refuses to serve because its history directory is missing would
            // take the plant down over a log file.
            eprintln!("[store] cannot create {}: {error}", cfg.dir.display());
        }
        let actor = Self {
            cfg,
            batch: String::new(),
            batch_day: None,
        };
        actor.sweep();
        actor
    }

    /// Delete day files older than the retention window. Called at startup and on a day
    /// rollover — never on a timer, because the set only changes when the day does.
    fn sweep(&self) {
        let today = day_key(Timestamp::now().as_second(), &self.cfg.zone);
        let Ok(entries) = fs::read_dir(&self.cfg.dir) else {
            return; // no directory yet: nothing to sweep
        };
        let days: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?.strip_suffix(".jsonl")?;
                name.parse::<Date>().ok().map(|_| name.to_string())
            })
            .collect();
        for day in prune(&days, &today, self.cfg.retention_days) {
            match fs::remove_file(self.cfg.dir.join(format!("{day}.jsonl"))) {
                Ok(()) => eprintln!("[store] removed {day}.jsonl (age > {} d)", self.cfg.retention_days),
                Err(e) => eprintln!("[store] could not remove {day}.jsonl: {e}"),
            }
        }
    }

    fn day_file(&self, day: &str) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.cfg.dir.join(format!("{day}.jsonl")))
    }

    /// Write the batch with one append and one sync. The batch is only cleared once the
    /// write succeeded, so a full disk keeps the rows in memory for the next attempt.
    fn flush(&mut self) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let day = self
            .batch_day
            .clone()
            .expect("a non-empty batch always knows its day");
        let mut file = self.day_file(&day)?;
        file.write_all(self.batch.as_bytes())?;
        file.sync_data()?;
        self.batch.clear();
        self.batch_day = None;
        Ok(())
    }

    fn append(&mut self, at: i64, line: &str) -> io::Result<()> {
        let day = day_key(at, &self.cfg.zone);
        // A row for a new day flushes and sweeps before joining the batch: otherwise one
        // batch would span midnight and land in the wrong file.
        if self.batch_day.as_deref().is_some_and(|buffered| buffered != day) {
            self.flush()?;
            self.sweep();
        }
        if self.batch.len() >= MAX_BATCH {
            self.flush()?;
        }
        if self.batch_day.is_none() {
            self.batch_day = Some(day);
        }
        self.batch.push_str(line);
        self.batch.push('\n');
        Ok(())
    }

    fn read_range(&mut self, from: i64, to: i64) -> io::Result<Vec<String>> {
        self.flush()?;
        let mut rows = Vec::new();
        for day in day_keys(from, to, &self.cfg.zone) {
            let path = self.cfg.dir.join(format!("{day}.jsonl"));
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            // A hard crash can leave half a row behind the last newline. Drop it here, so no
            // caller has to defend against a torn tail of its own format.
            let complete = match text.ends_with('\n') {
                true => text.as_str(),
                false => &text[..text.rfind('\n').map_or(0, |i| i + 1)],
            };
            rows.extend(complete.lines().filter(|line| !line.is_empty()).map(str::to_string));
        }
        Ok(rows)
    }
}

impl Message<AppendRow> for StoreActor {
    type Reply = io::Result<()>;

    async fn handle(
        &mut self,
        msg: AppendRow,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.append(msg.at, &msg.line)
    }
}

impl Message<Flush> for StoreActor {
    type Reply = io::Result<()>;

    async fn handle(
        &mut self,
        _msg: Flush,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.flush()
    }
}

impl Message<ReadRange> for StoreActor {
    type Reply = io::Result<Vec<String>>;

    async fn handle(
        &mut self,
        msg: ReadRange,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.read_range(msg.from, msg.to)
    }
}

/// Flush on a ticker, like the bus and planner drivers. The actor holds the timer's only
/// state, so a missed tick simply delays one flush.
pub async fn drive(
    store: kameo::actor::ActorRef<StoreActor>,
    every: Duration,
) -> anyhow::Result<()> {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match store.ask(Flush).await {
            Ok(()) => {}
            Err(kameo::error::SendError::HandlerError(e)) => eprintln!("[store] flush failed: {e}"),
            Err(_) => anyhow::bail!("store actor died"),
        }
    }
}

/// `{dir}`-relative day file name for an instant, in the store's zone.
fn day_key(ts: i64, zone: &TimeZone) -> String {
    Timestamp::from_second(ts)
        .expect("unix seconds always convert")
        .to_zoned(zone.clone())
        .date()
        .to_string()
}

/// Every day file name a `[from, to]` window can touch, oldest first.
fn day_keys(from: i64, to: i64, zone: &TimeZone) -> Vec<String> {
    let start = Timestamp::from_second(from)
        .expect("unix seconds always convert")
        .to_zoned(zone.clone())
        .date();
    let end = Timestamp::from_second(to)
        .expect("unix seconds always convert")
        .to_zoned(zone.clone())
        .date();
    let mut days = Vec::new();
    let mut day = start;
    while day <= end && (days.len() as i64) < MAX_RANGE_DAYS {
        days.push(day.to_string());
        let Ok(next) = day.checked_add(jiff::Span::new().days(1)) else {
            break;
        };
        day = next;
    }
    days
}

/// Which day files are past the retention window. Pure, so the policy is testable without
/// touching a clock or a filesystem.
fn prune(days: &[String], today: &str, keep: i64) -> Vec<String> {
    let Ok(today) = today.parse::<Date>() else {
        return Vec::new(); // a broken clock must not delete history
    };
    let Ok(horizon) = today.checked_sub(jiff::Span::new().days(keep)) else {
        return Vec::new();
    };
    days.iter()
        .filter_map(|day| day.parse::<Date>().ok().map(|date| (day.clone(), date)))
        .filter(|(_, date)| *date < horizon)
        .map(|(day, _)| day)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use kameo::actor::Spawn;

    use super::*;

    fn config(dir: &Path) -> StoreConfig {
        StoreConfig {
            dir: dir.to_path_buf(),
            retention_days: RETENTION_DAYS,
            zone: TimeZone::get("Europe/Prague").unwrap(),
        }
    }

    /// 2026-11-09 12:00 Prague.
    const NOON: i64 = 1_794_222_000;
    const DAY: i64 = 86_400;

    #[tokio::test]
    async fn rows_land_in_one_day_file_and_read_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let actor = StoreActor::spawn(StoreActor::new(config(dir.path())));

        for n in 0..3 {
            actor
                .ask(AppendRow {
                    at: NOON + n,
                    line: format!(r#"{{"n":{n}}}"#),
                })
                .await
                .unwrap();
        }
        actor.ask(Flush).await.unwrap();

        let path = dir.path().join("2026-11-09.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3, "one append, three rows: {text}");

        let rows = actor
            .ask(ReadRange {
                from: NOON - DAY,
                to: NOON + DAY,
            })
            .await
            .unwrap();
        assert_eq!(rows, vec![r#"{"n":0}"#, r#"{"n":1}"#, r#"{"n":2}"#]);

        // The rows that were only buffered are readable without an explicit flush.
        actor
            .ask(AppendRow {
                at: NOON + 10,
                line: r#"{"n":3}"#.to_string(),
            })
            .await
            .unwrap();
        let rows = actor
            .ask(ReadRange {
                from: NOON,
                to: NOON + 20,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 4);
    }

    #[tokio::test]
    async fn a_batch_never_spans_midnight() {
        let dir = tempfile::tempdir().unwrap();
        let actor = StoreActor::spawn(StoreActor::new(config(dir.path())));

        actor
            .ask(AppendRow {
                at: NOON,
                line: r#"{"late":true}"#.to_string(),
            })
            .await
            .unwrap();
        actor
            .ask(AppendRow {
                at: NOON + DAY,
                line: r#"{"early":true}"#.to_string(),
            })
            .await
            .unwrap();
        actor.ask(Flush).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("2026-11-09.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("2026-11-10.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    /// A power cut can leave half a row; the next read must not see it.
    #[tokio::test]
    async fn a_torn_tail_is_dropped_not_surfaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("2026-11-09.jsonl"),
            "{\"whole\":1}\n{\"torn\":",
        )
        .unwrap();
        let actor = StoreActor::spawn(StoreActor::new(config(dir.path())));
        let rows = actor
            .ask(ReadRange {
                from: NOON,
                to: NOON,
            })
            .await
            .unwrap();
        assert_eq!(rows, vec![r#"{"whole":1}"#]);
    }

    #[test]
    fn retention_keeps_the_window_and_drops_older_days() {
        let days = ["2026-01-01", "2026-09-09", "2026-09-10", "2026-11-09"]
            .map(str::to_string);
        // keep 61 days from 2026-11-09 → horizon 2026-09-09, so that day stays.
        assert_eq!(prune(&days, "2026-11-09", 61), vec!["2026-01-01".to_string()]);
        // keep 60 → horizon 2026-09-10.
        assert_eq!(
            prune(&days, "2026-11-09", 60),
            vec!["2026-01-01".to_string(), "2026-09-09".to_string()]
        );
        // A clock before the epoch must not wipe the directory.
        assert!(prune(&days, "not-a-date", 60).is_empty());
    }
}
