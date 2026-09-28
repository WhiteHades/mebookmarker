# mebookmarker

Archive your bookmarks from every medium, search them, and keep them.

One binary, one SQLite file, one TOML config. It reads from X, Reddit, Hacker
News, GitHub, RSS/Atom, YouTube, your browser's export, OPML, JSON, a list of
URLs, and a folder of notes. It writes to Markdown, Obsidian, HTML, CSV, JSONL,
JSON, OPML, and a raw archive. It runs in a terminal and it runs from a script.

```
mbm add https://example.com/article          # save a url
mbm import ~/Downloads/bookmarks.html        # read a browser export
mbm run                                     # fetch, enrich, export
mbm search "sqlite internals"                # find it
mbm tui                                     # browse it
```

## Why

Every bookmark tool is one of two things: a browser extension that saves urls and
gives them back, or a read-later service that owns your data. Neither one keeps
the text. They store `https://twitter.com/user/status/12345` and hope the link
still resolves, which it does not — X rate-limits, Reddit deletes, blogs rot, and
a link into a paywall becomes a title and nothing else.

This keeps the text. Every item that goes in comes out with a title, a summary,
the body, its links, its media, its author, and its date, in a format you can read
without this program. The JSONL export is lossless: a bookmark written to it and
read back is the same bookmark.

## The cost argument

Enrichment is where a tool like this usually gets expensive, because it reads like
it needs a model on every item. It does not. There are three tiers, and only the
cheap ones run by default.

| tier | what it is | cost | when it runs |
|------|-----------|------|--------------|
| 1 | deterministic: links, `@` mentions, `#` hashtags, bare domains, SimHash fingerprints, taxonomy rules | free, and local | every item, always |
| 2 | one typed question per item to the Vercel AI Gateway — a `choice` or a `boolean` against a list decided in advance | ~$0.00002 per item | every item, when a gateway key is set |
| 3 | a local coding agent writes a title, a summary, and image descriptions | your own subscription | only when `enrich.describe = true` |

Tier 2 is the part that matters. The model is never asked to write anything. It is
asked to pick a name from a list you wrote, and a probability comes back with it.
That is measurable, predictable, and cacheable. Measured against the gateway: 565
ms and $0.0000196 for three questions on one item, 466 tokens in and 83 out.

A hundred thousand bookmarks with the default switches costs about two dollars and
about three hours. Turning on `describe` makes it as expensive as whatever your
agent costs, which is why it is off unless asked for.

## Install

```sh
git clone https://github.com/WhiteHades/mebookmarker
cd mebookmarker
cargo build --release
install -m755 target/release/mbm ~/.local/bin/
```

Rust 1.88 or newer. The only external programs it may call are `opencode`,
`codex`, or `claude`, and only when the describe stage is on.

## Getting started

Nothing needs configuring. This works on a machine that has never run it:

```sh
mbm add https://www.sqlite.org/wal.html -t databases -n "the write-ahead log, finally understood"
mbm enrich -s entities
mbm search wal
mbm tui
```

The store lands in `$XDG_DATA_HOME/mebookmarker/mebookmarker.db`. `mbm config
init` writes a `mebookmarker.toml` next to it with every key and its default.

## Sources

Every source is one adapter behind the same port, and each one can be checked
before it runs.

| source | what it reads | needs |
|--------|---------------|-------|
| `x` | bookmarks, through x's own graphql endpoint | `auth_token` and `ct0` cookies |
| `x-bird` | the same, by shelling out to [`bird`](https://github.com/steipete/bird) | `bird` on the path |
| `reddit` | a subreddit's posts | a descriptive user agent |
| `hackernews` | stories, comments, ask hn, show hn | nothing |
| `github` | your stars | a token |
| `rss` | any feed, rss 2.0, rdf, or atom | nothing |
| `youtube` | a playlist | nothing |
| `json` | any of five exporter shapes | nothing |
| `opml` | a feed reader's subscriptions | nothing |
| `browser` | a netscape or html bookmark export | nothing |
| `local` | a folder of markdown, text, html, and opml | nothing |

A `mebookmarker.toml` with two sources:

```toml
[[sources]]
medium = "rss"
enabled = true

[sources.options]
urls = ["https://blog.rust-lang.org/feed.xml", "https://simonwillison.net/atom/everything/"]

[[sources]]
medium = "hackernews"
enabled = true

[sources.options]
tag = "show_hn"
```

Secrets are named, never stored. The config says which environment variable holds
a value, so the file stays safe to keep in a dotfiles repository:

```sh
export TWITTER_COOKIES="$(pbpaste)"     # a cookie jar, or `name=value` lines
export GITHUB_TOKEN=ghp_...
export AI_GATEWAY_API_KEY=vck_...
```

## Enrichment

Five stages, in the order that makes a run cheap. Each one keeps its own
timestamp on the row, so a run that dies half way leaves the rows it finished
stamped and the rows it did not untouched. There is no queue to lose and no
checkpoint file to go stale.

| stage | what it does | tier |
|-------|--------------|------|
| `entities` | links, mentions, hashtags, paywall marks, fingerprints | 1 |
| `vision` | alt text, and one `boolean` question about whether an image needs describing | 1–2 |
| `tags` | one `choice` question against the tag vocabulary | 2 |
| `categorize` | taxonomy rules first, then one `choice` question | 2 |
| `describe` | a title and a summary, written by a local agent | 3 |

```sh
mbm enrich                       # everything the config enables
mbm enrich -s entities -n 500     # one stage, five hundred rows
mbm enrich --status              # what is waiting
mbm enrich --redo tags           # put every bookmark back in that queue
```

`--redo` is how a new taxonomy gets applied to an archive that already exists:
clear the column, run the stage, and every row goes through it again.

## Output

Eight formats, all of them pure — a sink takes bookmarks and produces bytes.

| format | shape | good for |
|--------|-------|----------|
| `markdown` | one note per bookmark, plus a daily index | reading, and git |
| `obsidian` | the same, with frontmatter and wikilinks | an obsidian vault |
| `html` | one file, with a filter box, no javascript libraries | opening it anywhere |
| `csv` | one row per bookmark, a fixed column set | a spreadsheet |
| `jsonl` | one bookmark per line | the archive copy |
| `json` | one document | the archive copy, readable |
| `opml` | a folder tree | a feed reader |
| `archive` | the raw source payload, one file per item | re-parsing later |

```sh
mbm export                                    # the configured sinks
mbm export -f obsidian -o ~/vault/marks       # one of them, somewhere else
```

The `archive` format is the one worth keeping a copy of. It is not a rendering,
it is the bytes a source gave us, so a later version can re-parse an item with a
better parser and get back the item it would have produced.

## What is verified, and what is not

This matters more than a feature list, so it is stated plainly.

**Verified end to end, against the live services, with real data:**

- `hackernews` — fetched from the algolia api, enriched, searched, exported
- `rss` — the reader handles rss 2.0, rdf, and atom
- `add`, `import`, `search`, `list`, `show`, `tag`, `delete`, `stats`, `export`,
  `enrich`, `config`, `tui` — every command, driven against a real store
- the `tags` and `categorize` stages — against the live gateway, picking a real
  category and a real tag and saving both
- the `describe` stage — against a live `codex` invocation
- all eight output formats, written from a store of mixed real data

**Tested against recorded responses, but not against the live service:**

- the `x` graphql request half. the parsing half is covered by tests against
  recorded responses, because that is where the bugs live. the request half
  needs a session, so it is not covered by an automated test. the first run
  against the real endpoint is where it gets checked, and the adapter reports a
  changed response shape as an error rather than as an empty bookmark list,
  because a silent empty result is the worst thing it could do.
- `reddit`, `github`, `youtube`, and the browser and file readers — the parsing
  halves are covered; the request halves need an account or a cookie.

## Search

BM25 and a SimHash-LSH fuzzy pass, fused with reciprocal rank fusion, with
AND-then-OR semantics: a query with several words prefers items matching all of
them but never hides the ones matching some. A bigram bitset prefilter rejects
most of the index before FTS5 is asked anything.

```sh
mbm search "wal journal"                     # hybrid, the default
mbm search --rank exact "sqlite internals"   # bm25 only
mbm search --rank fuzzy "sqtlite"            # typo-tolerant
mbm search --json wal | jq '.[].url'         # for a script
```

`tab` cycles the mode in the terminal interface, because a half-typed word wants
fuzzy and a finished one does not.

## The terminal interface

```sh
mbm tui
```

Four views over one list: browse, search, detail, and tags. Everything is on the
keyboard, and the status line always says what the last action did.

```
 search Boonful                                                  hybrid
▎  2026-09-27    hacker-news      Show HN: Boonful — publish…   @atifhub
   2026-09-26    reddit          Ask HN: how do you keep a sma…  @havu12
   2026-09-25    markdown-file   AI alignment and interpretabi…  @trq212
 1 of 3 bookmarks    removed 13941… . ctrl-u puts it back.   ↑↓ move · ⏎ open …
```

| key | what it does |
|-----|--------------|
| `↑` `↓` `pgup` `pgdn` `home` `end` | move |
| `⏎` | open the item, or filter by the tag you are on |
| `esc` | go back to the list |
| `tab` | change the ranking: hybrid, exact, fuzzy |
| `f2` | tag the selected item |
| `ctrl-d` | remove it, with `ctrl-u` to put it back |
| `ctrl-u` | put back the last thing removed |
| `ctrl-g` | the tag list |
| `ctrl-k` | clear the query |
| `ctrl-r` | reload |
| `ctrl-c` | leave |

### How it looks

The interface reads the terminal it is in. `COLORFGBG` says whether the
background is light or dark and the palette is chosen to match, because a palette
tuned for one is unreadable on the other: the dark palette's secondary text is
9:1 on near-black and 1.9:1 on white. `MBM_THEME=light|dark|auto` overrides it,
`NO_COLOR` stops the interface asking for colour at all, and both are honoured
without the program reading a single byte of the terminal's input.

Every colour in it is a solved value rather than a chosen one, and the solver and
the measurement are both in the repository:

```sh
cargo run -p mbm-app --example contrast-check   # 38 pairs, both appearances
cargo run -p mbm-app --example solve-theme      # the walk that produced them
```

Each pair is measured two ways and has to clear both: the WCAG 2 ratio, which is
what a conformance claim rests on, and the APCA lightness contrast, which is the
number a palette is actually designed against. APCA is the stricter of the two
here by a wide margin, and the first hand-picked secondary text cleared 4.5:1
comfortably while only reaching Lc 40.

The list's columns give way as the terminal narrows, in the order of how much
each one says: the source goes first, then the date, and the title never goes,
because it is the only column that says what the row is. The frame itself is
checked at five widths by the end-to-end suite.

### How it moves

A terminal redraws a whole frame, so motion here is not free: every frame is a
full repaint. What is animated is therefore the rare thing, not the frequent one.

- a keystroke, a row moving, a frame appearing: instant. there is no transition
  on a high-frequency interaction, because a person typing is not waiting for a
  picture and paying for it in repaints is how a list feels laggy.
- opening an item, a status message landing, an empty state arriving: a 300ms
  entrance, a 150ms exit, a 100ms stagger between the semantic chunks of a view,
  on `cubic-bezier(0.2, 0, 0, 1)`.
- nothing animates on the first frame. the first thing drawn is the finished
  thing, and motion only ever answers something you did.
- every animated change leaves a static cue behind it. a row that slides in is
  also bold and carries a bar; a status that brightens and settles is also text
  that stays.
- `MBM_NO_MOTION=1` turns all of it off.

### Checking it

The interface is a program that draws, so the only honest way to check what it
drew is to let it draw.

```sh
# drive it in a pty and read the frame back
cargo test -p mbm-app --test e2e the_terminal

# render one frame, as text and as html carrying the real cell colours
cargo run -p mbm-app --example frame -- ~/.local/share/mebookmarker/data 100x30 /tmp/frame dark browse
```

## Command line

```
mbm add <url>...       save urls
mbm import <path>      read a file or a folder
mbm run                fetch, enrich, export
mbm search <query>     search
mbm show <id>          print one bookmark as json
mbm list               list the archive
mbm tag <id> <tag>     add or remove a tag by hand
mbm delete <id>        remove a bookmark
mbm stats              counts by medium, tag, and stage
mbm export             write the archive out
mbm enrich             run the enrichment stages
mbm config             read and write the configuration
mbm tui                the interactive browser
```

Exit codes are worth knowing about in a script: `0` success, `1` a failure worth
reading, `2` a credential or configuration problem that retrying will not fix,
`3` a rate limit or a transient failure that will.

## Where things live

| what | where |
|------|-------|
| the store | `$XDG_DATA_HOME/mebookmarker/mebookmarker.db` |
| the config | `$XDG_DATA_HOME/mebookmarker/mebookmarker.toml` |
| agent state | `~/.local/share/opencode`, `~/.codex`, `~/.claude` |

## How it is built

Nine crates, layered so each one is testable without the ones above it.

```
mbm-core     the domain: a bookmark, a taxonomy, a compiled matcher, the three ports
mbm-store    sqlite: schema, fts5, simhash, lsh, the prefilter, rrf search
mbm-extract  http with backoff, readability, link classification, oembed
mbm-jev      the gateway client, typed questions, cost accounting
mbm-ingest   one adapter per source
mbm-enrich   the five stages and the resumable pipeline
mbm-agent    drivers for opencode, codex, and claude
mbm-sink     the eight output formats
mbm-app      config, pipeline, cli, tui
```

Some things that were measured rather than assumed, and are commented where they
live so the next person does not re-run the experiment:

- **no FTS5 prefix index.** it costs 30% more space and turns a rare two-letter
  stem from 0.06 ms into 41 ms. the prefilter serves as-you-type instead.
- **no cap-then-rank.** bm25 returns `-0.0` without a `MATCH`, so the "fast"
  version was fast because it was wrong. the correct one is 1200 ms and correct.
- **no hand-written AVX2 case folding.** 0.11× against `to_ascii_lowercase`,
  measured, deleted.

## Licence

MIT. See [LICENSE](LICENSE).

---

A rewrite of [smaug](https://github.com/WhiteHades/smaug), which was MIT and is
credited in the licence file.
