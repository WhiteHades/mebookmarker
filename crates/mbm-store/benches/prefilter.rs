use mbm_store::fingerprint::BandIndex;
use mbm_store::prefilter::Prefilter;
use std::hint::black_box;
use std::time::Instant;

const DOCS: usize = 1_000_000;

const WORDS: &[&str] = &[
    "simd",
    "index",
    "allocator",
    "sqlite",
    "tokenizer",
    "fingerprint",
    "throughput",
    "latency",
    "bookmark",
    "archive",
    "embedding",
    "retrieval",
    "compiler",
    "borrow",
    "lifetime",
    "cache",
    "bandwidth",
    "pipeline",
    "scheduler",
    "allocator",
    "syscall",
    "page",
    "fault",
    "vector",
    "release",
    "profile",
    "benchmark",
    "kernel",
    "thread",
    "contention",
    "lock",
];

/// xorshift64*, so the corpus has a realistic vocabulary spread instead of
/// every document sharing the same words.
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

    fn word(&mut self) -> &'static str {
        WORDS[(self.next() % WORDS.len() as u64) as usize]
    }
}

fn synth(rng: &mut Rng, i: usize) -> String {
    let mut s = String::with_capacity(120);
    for _ in 0..12 {
        s.push_str(rng.word());
        s.push(' ');
    }
    s.push_str(&i.to_string());
    s
}

fn main() {
    let mut rng = Rng(0x243F_6A88_85A3_08D3);
    let docs: Vec<String> = (0..DOCS).map(|i| synth(&mut rng, i)).collect();
    let bytes: usize = docs.iter().map(String::len).sum();
    println!("docs={DOCS} text={:.1} MB", bytes as f64 / 1e6);

    let t = Instant::now();
    let mut index = Prefilter::new(DOCS);
    for (i, d) in docs.iter().enumerate() {
        index.insert(i, d);
    }
    let build = t.elapsed();
    println!(
        "prefilter build   {build:>10.2?}  ({:.0} docs/s, {} columns, {:.1} MB)",
        DOCS as f64 / build.as_secs_f64(),
        index.column_count(),
        index.size_bytes() as f64 / 1e6,
    );

    for q in ["simd throughput", "sqlite tokenizer", "github benchmarked"] {
        let t = Instant::now();
        let hits = index.candidates(q);
        let union = t.elapsed();
        let t = Instant::now();
        let hits_all = index.candidates_all(q);
        let inter = t.elapsed();
        println!(
            "  {q:<22} union {:>8.2?} ({:>7} hits)   intersect {:>8.2?} ({:>7} hits)",
            union,
            hits.len(),
            inter,
            hits_all.len(),
        );
        black_box(hits);
    }

    let t = Instant::now();
    let mut bands = BandIndex::new();
    for (i, d) in docs.iter().enumerate() {
        bands.insert(i as u64, mbm_store::fingerprint::fingerprint(d.split(' ')));
    }
    let band_build = t.elapsed();
    println!("band index build  {band_build:>10.2?}  ({} entries)", bands.len());

    let fp = mbm_store::fingerprint::fingerprint("simd throughput on sqlite".split(' '));
    let t = Instant::now();
    let near = bands.find_near(fp, 3);
    println!("find_near         {:>10.2?}  ({} candidates)", t.elapsed(), near.len());
}
