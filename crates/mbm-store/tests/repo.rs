//! repository tests.
//!
//! these run against a real sqlite database rather than a mock, because the
//! behaviour worth checking here is the sql itself: unique-index enforcement,
//! trigger ordering, partial-index selection, and keyset pagination.

use ahash::AHashSet;
use mbm_core::bookmark::{Assigner, Bookmark, CategoryAssignment, Link, MediaKind};
use mbm_core::category::{Action, Category, CategoryRule, Taxonomy};
use mbm_core::bookmark::ThreadRole;
use mbm_core::medium::{LinkKind, SourceMedium};
use mbm_core::{Author, Id, SourceRef};
use rusqlite::Connection;
use url::Url;

use mbm_store::{Filter, Mode, Repo, Searcher, migrate};

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    migrate(&conn).unwrap();
    conn
}

fn sample(external_id: &str) -> Bookmark {
    let mut b = Bookmark::new(
        SourceRef::new(SourceMedium::X, external_id, Url::parse("https://x.com/i/status/1").ok()),
        "a post about rust and simd",
        1_700_000_000_000,
    );
    b.author = Some(Author::new("@SimonW").with_name("Simon Willison"));
    b.created_at = Some(1_700_000_000_000);
    b.role = Some(ThreadRole::Quote);
    b.push_tag("rust").push_tag("simd");

    b.links.push(Link {
        original: Url::parse("https://t.co/abc").unwrap(),
        resolved: Url::parse("https://github.com/simonw/llm").unwrap(),
        kind: LinkKind::Repository,
        title: Some("simonw/llm".into()),
        body: Some("a library".into()),
        summary: None,
        blocked: None,
    });
    b.media.push(mbm_core::bookmark::Media {
        kind: MediaKind::Photo,
        url: Url::parse("https://pbs.twimg.com/a.jpg").unwrap(),
        preview_url: None,
        width: Some(1200),
        height: Some(800),
        duration_ms: None,
        alt_text: None,
    });
    b
}

#[test]
fn a_bookmark_round_trips_through_the_store() {
    let conn = db();
    let repo = Repo::new(&conn);
    let original = sample("123");
    repo.insert(&original).unwrap();

    let loaded = repo.load(original.id).unwrap().expect("row should exist");
    assert_eq!(loaded.text, original.text);
    assert_eq!(loaded.source.external_id, "123");
    assert_eq!(loaded.author.as_ref().unwrap().handle, "simonw");
    assert_eq!(loaded.created_at, Some(1_700_000_000_000));
    assert_eq!(loaded.role, Some(ThreadRole::Quote));
}

#[test]
fn satellites_survive_the_round_trip() {
    let conn = db();
    let repo = Repo::new(&conn);
    let original = sample("123");
    repo.insert(&original).unwrap();

    let loaded = repo.load(original.id).unwrap().unwrap();
    assert_eq!(loaded.links.len(), 1);
    assert_eq!(loaded.links[0].kind, LinkKind::Repository);
    assert_eq!(loaded.links[0].title.as_deref(), Some("simonw/llm"));
    assert_eq!(loaded.media.len(), 1);
    assert_eq!(loaded.media[0].width, Some(1200));
    assert_eq!(loaded.media[0].kind, MediaKind::Photo);
}

#[test]
fn re_inserting_the_same_source_item_is_rejected_by_the_index() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut a = sample("123");
    repo.insert(&a).unwrap();
    a.id = Id::now();
    assert!(repo.insert(&a).is_err(), "the unique index must reject a second row");
}

#[test]
fn upsert_reports_whether_it_wrote() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("123");
    let (first_id, wrote) = repo.upsert(&a).unwrap();
    assert!(wrote);
    assert_eq!(first_id, a.id);

    let mut b = sample("123");
    b.id = Id::now();
    b.text = "different text entirely".into();
    let (second_id, wrote) = repo.upsert(&b).unwrap();
    assert!(!wrote);
    assert_eq!(first_id, second_id, "an upsert must return the original id");
    assert_eq!(repo.count().unwrap(), 1);
}

#[test]
fn insert_many_skips_what_is_already_there() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("1");
    let b = sample("2");
    assert_eq!(repo.insert_many([&a, &b]).unwrap(), 2);
    assert_eq!(repo.insert_many([&a, &b]).unwrap(), 0, "a re-run writes nothing new");
    assert_eq!(repo.count().unwrap(), 2);
}

#[test]
fn the_identity_index_covers_different_media_separately() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut a = sample("same-id");
    repo.insert(&a).unwrap();
    a.id = Id::now();
    a.source.medium = SourceMedium::Reddit;
    assert!(repo.insert(&a).is_ok(), "the same external id from another medium is a different item");
}

#[test]
fn the_search_index_is_filled_by_inserting() {
    let conn = db();
    let repo = Repo::new(&conn);
    repo.insert(&sample("123")).unwrap();
    let hits = Searcher::new(&conn).search("simd", Mode::Exact, 10).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn keyset_pagination_walks_the_corpus_once() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut ids = Vec::new();
    for i in 0..10 {
        let mut b = sample(&format!("e{i}"));
        b.id = Id::from_parts(1_700_000_000_000 + i as u64, 0);
        b.enrichment.entities_at = None;
        repo.insert(&b).unwrap();
        ids.push(b.id);
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = repo.pending_ids("entities_at", cursor, 3).unwrap();
        if page.is_empty() {
            break;
        }
        seen.extend(page.iter().copied());
        cursor = page.last().copied();
    }
    assert_eq!(seen, ids, "every row exactly once, in id order");
}

#[test]
fn pending_counts_only_unfinished_rows() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut done = sample("done");
    done.enrichment.entities_at = Some(1);
    repo.insert(&done).unwrap();
    repo.insert(&sample("todo1")).unwrap();
    repo.insert(&sample("todo2")).unwrap();
    assert_eq!(repo.pending("entities_at").unwrap(), 2);
    assert_eq!(repo.pending("described_at").unwrap(), 3);
}

#[test]
fn marking_a_stage_clears_it_from_the_queue() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("1");
    repo.insert(&a).unwrap();
    assert_eq!(repo.pending("entities_at").unwrap(), 1);
    repo.mark_stage("entities_at", &[a.id], 999).unwrap();
    assert_eq!(repo.pending("entities_at").unwrap(), 0);
    let loaded = repo.load(a.id).unwrap().unwrap();
    assert_eq!(loaded.enrichment.entities_at, Some(999));
}

#[test]
fn the_partial_index_keeps_pending_queries_off_the_main_table() {
    let conn = db();
    let repo = Repo::new(&conn);
    for i in 0..500 {
        let mut b = sample(&format!("e{i}"));
        if i % 2 == 0 {
            b.enrichment.entities_at = Some(1);
        }
        repo.insert(&b).unwrap();
    }
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN SELECT id FROM bookmark WHERE entities_at IS NULL ORDER BY id LIMIT 10",
            [],
            |r| r.get(3),
        )
        .unwrap();
    assert!(plan.contains("bookmark_pending"), "expected the partial index, got: {plan}");
}

#[test]
fn tags_are_replaced_wholesale() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("1");
    repo.insert(&a).unwrap();
    repo.set_tags(a.id, &AHashSet::from(["one".to_owned(), "two".to_owned()]))
        .unwrap();
    let tags = repo.tags_with_counts(10).unwrap();
    assert_eq!(tags.len(), 2);
    repo.set_tags(a.id, &AHashSet::from(["three".to_owned()])).unwrap();
    assert_eq!(repo.tags_with_counts(10).unwrap().len(), 1);
}

#[test]
fn tags_are_counted_by_frequency() {
    let conn = db();
    let repo = Repo::new(&conn);
    for i in 0..3 {
        let mut b = sample(&format!("common{i}"));
        b.push_tag("common");
        b.push_tag(format!("rare{i}"));
        repo.insert(&b).unwrap();
    }
    let tags = repo.tags_with_counts(10).unwrap();
    assert_eq!(tags[0], ("common".to_owned(), 3));
}

#[test]
fn categories_sync_and_assign() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut taxonomy = Taxonomy::empty();
    taxonomy.insert(Category::new("tool", "Tool", "#06b6d4", "A tool.").with_action(Action::File));
    taxonomy.insert(Category::new("general", "General", "#64748b", "Anything."));
    let ids = repo.sync_categories(&taxonomy).unwrap();
    assert_eq!(ids.len(), 2);

    let a = sample("1");
    repo.insert(&a).unwrap();
    repo.set_categories(
        a.id,
        &[
            CategoryAssignment { slug: "tool".into(), confidence: 0.9, assigned_by: Assigner::Jev },
            CategoryAssignment { slug: "general".into(), confidence: 0.4, assigned_by: Assigner::Rule },
        ],
        &ids,
    )
    .unwrap();

    let loaded = repo.load(a.id).unwrap().unwrap();
    assert_eq!(loaded.categories.len(), 2);
    assert_eq!(loaded.categories[0].slug, "tool", "highest confidence comes first");
    assert_eq!(loaded.categories[0].assigned_by, Assigner::Jev);
}

#[test]
fn an_assignment_naming_an_unknown_category_is_dropped() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("1");
    repo.insert(&a).unwrap();
    repo.set_categories(
        a.id,
        &[CategoryAssignment { slug: "nope".into(), confidence: 1.0, assigned_by: Assigner::Jev }],
        &ahash::AHashMap::new(),
    )
    .unwrap();
    assert!(repo.load(a.id).unwrap().unwrap().categories.is_empty());
}

#[test]
fn syncing_categories_twice_updates_rather_than_duplicates() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut taxonomy = Taxonomy::empty();
    taxonomy.insert(Category::new("tool", "Tool", "#000000", "Old."));
    repo.sync_categories(&taxonomy).unwrap();
    taxonomy.insert(Category::new("tool", "Tool", "#ffffff", "New."));
    let ids = repo.sync_categories(&taxonomy).unwrap();
    let (color, description): (String, String) = conn
        .query_row("SELECT color, description FROM category WHERE slug = 'tool'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(color, "#ffffff");
    assert_eq!(description, "New.");
    assert_eq!(ids.len(), 1);
}

#[test]
fn fingerprints_are_stored_as_signed_integers() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("1");
    repo.insert(&a).unwrap();
    // a value with the high bit set would overflow a signed column
    repo.set_fingerprint(a.id, u64::MAX).unwrap();
    assert_eq!(repo.load(a.id).unwrap().unwrap().fingerprint, Some(u64::MAX));
}

#[test]
fn the_band_index_loads_from_the_store() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("1");
    repo.insert(&a).unwrap();
    repo.set_fingerprint(a.id, 0xDEAD_BEEF_1234_5678).unwrap();
    let index = mbm_store::fingerprint::BandIndex::load(&conn).unwrap();
    assert_eq!(index.len(), 1);
    assert_eq!(index.fingerprint(a.id.get()), Some(0xDEAD_BEEF_1234_5678));
}

#[test]
fn a_filter_narrows_by_tag() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut tagged = sample("1");
    tagged.push_tag("rust");
    repo.insert(&tagged).unwrap();
    let mut untagged = sample("2");
    untagged.tags.clear();
    repo.insert(&untagged).unwrap();

    let filter = Filter { tag: Some("rust".into()), ..Filter::default() };
    let found = repo.query(&filter, 10, 0).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(repo.count_matching(&filter).unwrap(), 1);
}

#[test]
fn a_filter_narrows_by_category() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut taxonomy = Taxonomy::empty();
    taxonomy.insert(Category::new("tool", "Tool", "#06b6d4", "A tool."));
    let ids = repo.sync_categories(&taxonomy).unwrap();

    let a = sample("1");
    repo.insert(&a).unwrap();
    repo.set_categories(
        a.id,
        &[CategoryAssignment { slug: "tool".into(), confidence: 0.9, assigned_by: Assigner::Jev }],
        &ids,
    )
    .unwrap();
    let b = sample("2");
    repo.insert(&b).unwrap();

    let filter = Filter { categories: vec!["tool".into()], ..Filter::default() };
    assert_eq!(repo.query(&filter, 10, 0).unwrap().len(), 1);
}

#[test]
fn a_filter_narrows_by_date_range() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut old = sample("old");
    old.created_at = Some(1_000);
    repo.insert(&old).unwrap();
    let mut new = sample("new");
    new.created_at = Some(9_000);
    repo.insert(&new).unwrap();

    let filter = Filter { since: Some(5_000), until: Some(10_000), ..Filter::default() };
    let found = repo.query(&filter, 10, 0).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].source.external_id, "new");
}

#[test]
fn a_filter_matches_text_across_title_and_body() {
    let conn = db();
    let repo = Repo::new(&conn);
    repo.insert(&sample("1")).unwrap();
    for query in ["simd", "rust", "github.com"] {
        let filter = Filter { text: Some(query.into()), ..Filter::default() };
        assert!(!repo.query(&filter, 10, 0).unwrap().is_empty(), "`{query}` found nothing");
    }
}

#[test]
fn an_empty_filter_matches_everything() {
    let conn = db();
    let repo = Repo::new(&conn);
    for i in 0..5 {
        repo.insert(&sample(&format!("e{i}"))).unwrap();
    }
    let filter = Filter::default();
    assert_eq!(repo.query(&filter, 10, 0).unwrap().len(), 5);
    assert_eq!(repo.count_matching(&filter).unwrap(), 5);
}

#[test]
fn like_wildcards_in_a_filter_are_escaped() {
    let conn = db();
    let repo = Repo::new(&conn);
    repo.insert(&sample("1")).unwrap();
    // a bare % would otherwise match every row
    let filter = Filter { text: Some("%%%".into()), ..Filter::default() };
    assert!(repo.query(&filter, 10, 0).unwrap().is_empty());
}

#[test]
fn list_pages_in_reverse_chronological_order() {
    let conn = db();
    let repo = Repo::new(&conn);
    for (i, at) in [3_000_i64, 1_000, 2_000].into_iter().enumerate() {
        let mut b = sample(&format!("e{i}"));
        b.created_at = Some(at);
        repo.insert(&b).unwrap();
    }
    let all = repo.list(10, 0).unwrap();
    let times: Vec<i64> = all.iter().filter_map(|b| b.created_at).collect();
    assert_eq!(times, vec![3_000, 2_000, 1_000]);
}

#[test]
fn load_ranked_preserves_the_order_it_was_given() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("1");
    let b = sample("2");
    repo.insert(&a).unwrap();
    repo.insert(&b).unwrap();
    let loaded = repo.load_ranked(&[b.id, a.id]).unwrap();
    assert_eq!(loaded[0].source.external_id, "2");
    assert_eq!(loaded[1].source.external_id, "1");
}

#[test]
fn loading_a_missing_bookmark_is_none_not_an_error() {
    let conn = db();
    let repo = Repo::new(&conn);
    assert!(repo.load(Id::now()).unwrap().is_none());
}

#[test]
fn a_described_title_reaches_the_search_index() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("1");
    repo.insert(&a).unwrap();
    repo.set_described(a.id, Some("A Better Title"), Some("a summary")).unwrap();
    let hits = Searcher::new(&conn).search("better", Mode::Exact, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(repo.load(a.id).unwrap().unwrap().title.as_deref(), Some("A Better Title"));
}

#[test]
fn indexed_extra_text_is_searchable() {
    let conn = db();
    let repo = Repo::new(&conn);
    let a = sample("1");
    repo.insert(&a).unwrap();
    repo.set_indexed_text(a.id, None, "recognised-tools").unwrap();
    let hits = Searcher::new(&conn).search("recognised", Mode::Exact, 10).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn by_tag_returns_only_tagged_bookmarks() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut a = sample("1");
    a.push_tag("wanted");
    repo.insert(&a).unwrap();
    repo.insert(&sample("2")).unwrap();
    assert_eq!(repo.by_tag("wanted", 10, 0).unwrap().len(), 1);
}

#[test]
fn a_whole_aggregate_survives_insert_and_reload_with_categories() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut taxonomy = Taxonomy::empty();
    taxonomy.insert(Category::new("tool", "Tool", "#06b6d4", "A tool."));
    taxonomy.insert(Category::new("article", "Article", "#ec4899", "Prose."));
    taxonomy.rules.push(CategoryRule::any("tool", ["github.com"]));
    let ids = repo.sync_categories(&taxonomy).unwrap();

    let a = sample("123");
    repo.insert(&a).unwrap();
    repo.set_categories(
        a.id,
        &[CategoryAssignment { slug: "tool".into(), confidence: 0.95, assigned_by: Assigner::Rule }],
        &ids,
    )
    .unwrap();

    let loaded = repo.load(a.id).unwrap().unwrap();
    assert_eq!(loaded.links.len(), 1);
    assert_eq!(loaded.media.len(), 1);
    assert_eq!(loaded.categories.len(), 1);
    assert_eq!(loaded.categories[0].assigned_by, Assigner::Rule);
}

#[test]
fn the_assigner_label_round_trips_through_the_database() {
    let conn = db();
    let repo = Repo::new(&conn);
    let mut taxonomy = Taxonomy::empty();
    taxonomy.insert(Category::new("tool", "Tool", "#06b6d4", "A tool."));
    let ids = repo.sync_categories(&taxonomy).unwrap();
    let a = sample("1");
    repo.insert(&a).unwrap();
    repo.set_categories(
        a.id,
        &[CategoryAssignment { slug: "tool".into(), confidence: 1.0, assigned_by: Assigner::Jev }],
        &ids,
    )
    .unwrap();
    let stored: String = conn
        .query_row("SELECT assigned_by FROM bookmark_category WHERE bookmark = ?1", [a.id.get() as i64], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, "jev", "serde writes the kebab-case variant name");
    assert_eq!(repo.load(a.id).unwrap().unwrap().categories[0].assigned_by, Assigner::Jev);
}
