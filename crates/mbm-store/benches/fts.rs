//! where the time actually goes on a full-text query.
//!
//! the corpus uses a zipfian vocabulary, because a 32-word corpus makes every
//! term match a sixth of the collection and turns a query benchmark into a
//! measure of how fast sqlite can sort six figures of rows. real text has a
//! long tail where most terms match a handful of documents.
//!
//! four strategies over the same index:
//!
//! - `order by rank`: what a naive implementation does. ranks every match.
//! - `cap then rank`: take a bounded slice of matches, rank only those.
//! - `and semantics`: require every term, so posting lists intersect.
//! - `prefix index`: with and without, to price the index honestly.

use rusqlite::Connection;
use std::hint::black_box;
use std::time::{Duration, Instant};

const DOCS: usize = 200_000;
const VOCAB: usize = 20_000;

/// zipf exponent. 1.0 gives a long-tailed distribution: the top few words are
/// common and most words appear a handful of times.
const ZIPF: f64 = 1.05;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// draw a rank from a zipf distribution over `VOCAB` words.
    fn word(&mut self) -> usize {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        let rank = ((1.0 - u).ln() / -ZIPF.ln()).exp().floor() as usize;
        rank.clamp(1, VOCAB) - 1
    }
}

fn corpus() -> Vec<(String, String, String)> {
    let mut rng = Rng(0x243F_6A88_85A3_08D3);
    (0..DOCS)
        .map(|i| {
            let mut body = String::with_capacity(160);
            for _ in 0..14 {
                body.push_str(&format!("w{} ", rng.word()));
            }
            let title = format!("w{} w{}", rng.word(), rng.word());
            let extra = format!("w{} {}", rng.word(), i % 97);
            (title, body, extra)
        })
        .collect()
}

fn open(prefix: Option<&str>) -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA page_size = 4096; PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF;")
        .unwrap();
    db.execute_batch(
        "CREATE TABLE bookmark(id INTEGER PRIMARY KEY, title TEXT, body TEXT, extra TEXT);
         CREATE VIRTUAL TABLE search USING fts5(
            title, body, extra, content='bookmark', content_rowid='id',
            tokenize='porter unicode61 remove_diacritics 2');",
    )
    .unwrap();
    if let Some(p) = prefix {
        db.execute_batch(&format!("DROP TABLE search;")).ok();
        db.execute_batch(&format!(
            "CREATE VIRTUAL TABLE search USING fts5(
                title, body, extra, content='bookmark', content_rowid='id',
                tokenize='porter unicode61 remove_diacritics 2', prefix='{p}');"
        ))
        .unwrap();
    }
    db.execute_batch(
        "CREATE TRIGGER ai AFTER INSERT ON bookmark BEGIN
            INSERT INTO search(rowid, title, body, extra) VALUES (new.id, new.title, new.body, new.extra);
         END;",
    )
    .unwrap();
    db
}

/// common, mid, and rare terms under the zipf draw above.
const COMMON: &[&str] = &["w0", "w1", "w2"];
const MID: &[&str] = &["w40", "w41", "w42"];
const RARE: &[&str] = &["w9000", "w9001", "w9002"];
const PAIR: &[&str] = &["w100", "w2500"];

fn main() {
    let rows = corpus();
    println!("{DOCS} docs, {VOCAB} word zipfian vocabulary\n");

    for (label, prefix) in [("no prefix index", None), ("prefix='2 3'", Some("2 3"))] {
        let db = open(prefix);
        let t = Instant::now();
        let tx = db.unchecked_transaction().unwrap();
        {
            let mut ins = tx
                .prepare("INSERT INTO bookmark(id, title, body, extra) VALUES (?1,?2,?3,?4)")
                .unwrap();
            for (i, (a, b, c)) in rows.iter().enumerate() {
                ins.execute(rusqlite::params![i as i64 + 1, a, b, c]).unwrap();
            }
        }
        tx.commit().unwrap();
        let build = t.elapsed();

        let size: i64 = db
            .query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| r.get(0))
            .unwrap();

        println!("{label}: build {build:.2?}, {size} bytes ({:.0} B/doc)", size as f64 / DOCS as f64);
        for (group, terms) in [("common", COMMON), ("mid", MID), ("rare", RARE)] {
            let hits: i64 = db
                .query_row("SELECT count(*) FROM search WHERE search MATCH ?1", [terms[0]], |r| r.get(0))
                .unwrap();
            let ranked = time(&db, "SELECT rowid FROM search WHERE search MATCH ?1 ORDER BY rank LIMIT 20", terms[0]);
            let capped = time_capped(&db, terms[0]);
            let subq = time(
                &db,
                "SELECT rowid, bm25(search, 3.0, 1.0, 1.5) FROM search
                 WHERE rowid IN (SELECT rowid FROM search WHERE search MATCH ?1 LIMIT 20000)
                 ORDER BY rank LIMIT 20",
                terms[0],
            );
            let prefix_q = time(&db, "SELECT rowid FROM search WHERE search MATCH ?1 ORDER BY rank LIMIT 20", &format!("{}*", &terms[0][..2]));
            println!(
                "  {group:<7} {:>7} hits | rank-all {:>8.2?}ms | cap+rank(loop) {:>8.2?}ms | cap+rank(1stmt) {:>8.2?}ms | prefix {:>8.2?}ms",
                hits,
                ranked.as_secs_f64() * 1000.0,
                capped.as_secs_f64() * 1000.0,
                subq.as_secs_f64() * 1000.0,
                prefix_q.as_secs_f64() * 1000.0,
            );
        }

        let single = time(&db, "SELECT rowid FROM search WHERE search MATCH ?1 ORDER BY rank LIMIT 20", PAIR[0]);
        let both = time(&db, "SELECT rowid FROM search WHERE search MATCH ?1 ORDER BY rank LIMIT 20", &format!("{} AND {}", PAIR[0], PAIR[1]));
        println!("  and: one term {:.2?}ms, both terms {:.2?}ms\n", single.as_secs_f64() * 1000.0, both.as_secs_f64() * 1000.0);
    }
}

fn time(db: &Connection, sql: &str, arg: &str) -> Duration {
    let mut stmt = db.prepare(sql).unwrap();
    let t = Instant::now();
    let n = stmt.query_map([arg], |r| r.get::<_, i64>(0)).unwrap().count();
    let d = t.elapsed();
    black_box(n);
    d
}

/// take a bounded slice of matches from the index, then rank only those.
///
/// the index is ordered by docid, so a capped scan is a straight range walk.
/// the score is then computed for a few hundred rows instead of every match,
/// which is where a common term's cost actually lives.
fn time_capped(db: &Connection, arg: &str) -> Duration {
    let t = Instant::now();
    let mut ids = Vec::with_capacity(2000);
    {
        let mut stmt = db
            .prepare("SELECT rowid FROM search WHERE search MATCH ?1 LIMIT 2000")
            .unwrap();
        for row in stmt.query_map([arg], |r| r.get::<_, i64>(0)).unwrap() {
            ids.push(row.unwrap());
        }
    }
    let mut best: Vec<(f64, i64)> = Vec::with_capacity(ids.len());
    {
        let mut stmt = db
            .prepare("SELECT rowid, bm25(search, 3.0, 1.0, 1.5) FROM search WHERE rowid = ?1")
            .unwrap();
        for id in &ids {
            if let Ok(row) = stmt.query_row([id], |r| Ok((r.get::<_, f64>(1)?, *id))) {
                best.push(row);
            }
        }
    }
    best.sort_by(|a, b| a.0.total_cmp(&b.0));
    best.truncate(20);
    let d = t.elapsed();
    black_box(best.len());
    d
}
