//! Near-duplicate entity deduplication for the assembled knowledge graph.
//!
//! Ported from the Python `graphify.dedup` module. Runs after clustering so
//! it can use community assignments as a same-topic tiebreaker.
//!
//! Pipeline (mirrors the Python version):
//!
//! 1. **ID pre-dedup** — first occurrence of each node id wins.
//! 2. **Exact normalisation pass** — nodes whose normalised labels match
//!    within the same source file are merged.
//! 3. **MinHash + LSH blocking** — high-entropy candidate labels are hashed
//!    into a band-LSH index; each node queries for neighbours.
//! 4. **Jaro-Winkler verification** — candidates with score ≥ `MERGE_THRESHOLD`
//!    are merged, subject to guards (variant suffixes, prefix extension,
//!    diverging numeric tokens, cross-file file-anchored blocking, exact
//!    label across files).
//! 5. **Optional LLM tiebreaker** — pairs in `[LLM_LOW, LLM_HIGH)` are
//!    surfaced for a follow-up LLM decision. In this port the pairs are
//!    logged but not yet sent to the model; the flag is present so the
//!    plumbing exists.
//!
//! Only surviving nodes are returned; edges are rewired to their group
//! survivor and self-loops introduced by merges are dropped.

use std::collections::{BTreeMap, HashMap, HashSet};

use graphify_core::model::{GraphEdge, GraphNode};

// ── constants ────────────────────────────────────────────────────────────────

const ENTROPY_THRESHOLD: f64 = 2.5;
const LSH_THRESHOLD: f64 = 0.7;
const MERGE_THRESHOLD: f64 = 92.0;
const COMMUNITY_BOOST: f64 = 5.0;
const NUM_PERM: usize = 128;
const LLM_LOW: f64 = 75.0;
const LLM_HIGH: f64 = 92.0;
const SHORT_LABEL_MAX: usize = 12;

const CHUNK_SUFFIX_MARKER: &str = "_c";

/// Node file_type values whose identity is anchored to their source location
/// rather than their label text. Mirror of Python's `_FILE_ANCHORED_NONCODE`.
const FILE_ANCHORED_NONCODE: &[&str] = &["rationale", "document"];

/// Outcome of a single dedup run — surfaced to the caller for reporting.
#[derive(Debug, Default, Clone)]
pub struct DedupStats {
    pub exact_merges: usize,
    pub fuzzy_merges: usize,
    pub ambiguous_pairs: usize,
    pub id_collisions: usize,
}

// ── MinHash ──────────────────────────────────────────────────────────────────

/// Mersenne prime used by the datasketch hash family. Matches the Python
/// port so signature quality is equivalent regardless of language.
const MERSENNE_PRIME: u64 = (1u64 << 61) - 1;
const HASH_MASK: u64 = 0xFFFF_FFFF;
/// Sentinel value for an "empty" MinHash slot — the largest 32-bit value.
const HASH_MAX: u64 = HASH_MASK;

/// Deterministic hash coefficients shared across all MinHash instances so
/// signatures generated in different files are comparable.
struct MinHashCoeffs {
    a: Vec<u64>,
    b: Vec<u64>,
}

impl MinHashCoeffs {
    fn new(num_perm: usize) -> Self {
        // Seeded LCG (Numerical Recipes constants) — deterministic across
        // runs and platforms, so two signatures over the same shingle set
        // are always identical.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            state
        };
        let mut a = Vec::with_capacity(num_perm);
        let mut b = Vec::with_capacity(num_perm);
        for _ in 0..num_perm {
            // a ∈ [1, MP), b ∈ [0, MP).
            a.push(1 + next() % (MERSENNE_PRIME - 1));
            b.push(next() % MERSENNE_PRIME);
        }
        Self { a, b }
    }
}

/// A MinHash sketch over a set of shingles.
///
/// Two sketches with the same underlying set produce the same signature; the
/// Jaccard similarity of the sets is approximated by the fraction of slots
/// on which their signatures agree.
struct MinHash<'c> {
    hashvalues: Vec<u64>,
    coeffs: &'c MinHashCoeffs,
}

impl<'c> MinHash<'c> {
    fn new(coeffs: &'c MinHashCoeffs) -> Self {
        Self {
            hashvalues: vec![HASH_MAX; coeffs.a.len()],
            coeffs,
        }
    }

    fn update(&mut self, v: &[u8]) {
        // Underlying hash: 32-bit SipHash. Any well-distributed 32-bit hash
        // works — datasketch uses SHA1[:4], but the choice is arbitrary since
        // MinHash quality depends only on uniform distribution.
        let hv = short_hash(v) as u64;
        for i in 0..self.hashvalues.len() {
            let phv = (self.coeffs.a[i]
                .wrapping_mul(hv)
                .wrapping_add(self.coeffs.b[i]))
                % MERSENNE_PRIME;
            let phv = phv & HASH_MASK;
            if phv < self.hashvalues[i] {
                self.hashvalues[i] = phv;
            }
        }
    }
}

fn short_hash(v: &[u8]) -> u32 {
    // Determinism matters for MinHash reproducibility, so route to a fixed
    // hash rather than `std::hash::DefaultHasher` (which is per-process seeded).
    fixed_hash(v)
}

/// Deterministic non-cryptographic 32-bit hash (xxHash32-like folding).
///
/// Chosen for reproducibility across runs — Rust's `DefaultHasher` seeds
/// itself per-process, which would produce different MinHash signatures on
/// every build.
fn fixed_hash(v: &[u8]) -> u32 {
    let mut h: u32 = 0x9E37_79B1;
    for &b in v {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h ^= (v.len() as u32).wrapping_mul(0x27D4_EB2F);
    h ^= h >> 15;
    h = h.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 13;
    h = h.wrapping_mul(0xC2B2_AE35);
    h ^ (h >> 16)
}

// ── LSH ──────────────────────────────────────────────────────────────────────

/// Numerical integration (trapezoidal) so we can pick `(bands, rows)` without
/// pulling in a scipy-equivalent.
fn integrate<F: Fn(f64) -> f64>(f: F, lo: f64, hi: f64, n: usize) -> f64 {
    let h = (hi - lo) / n as f64;
    (0..n).map(|i| f(lo + i as f64 * h)).sum::<f64>() * h
}

/// Find `(bands, rows)` that minimise the weighted false-positive plus
/// false-negative error for the LSH s-curve at `threshold`. Same trade-off
/// as `datasketch`'s optimal parameter search.
fn optimal_lsh_params(threshold: f64, num_perm: usize) -> (usize, usize) {
    let mut best_err = f64::INFINITY;
    let mut best = (1, 1);
    for b in 1..=num_perm {
        let max_r = num_perm / b;
        if max_r == 0 {
            break;
        }
        for r in 1..=max_r {
            let bf = b as f64;
            let rf = r as f64;
            let fp = integrate(|s| 1.0 - (1.0 - s.powf(rf)).powf(bf), 0.0, threshold, 128);
            let fn_ = integrate(
                |s| 1.0 - (1.0 - (1.0 - s.powf(rf)).powf(bf)),
                threshold,
                1.0,
                128,
            );
            let err = 0.5 * fp + 0.5 * fn_;
            if err < best_err {
                best_err = err;
                best = (b, r);
            }
        }
    }
    best
}

struct MinHashLsh {
    b: usize,
    r: usize,
    tables: Vec<HashMap<Vec<u64>, Vec<String>>>,
}

impl MinHashLsh {
    fn new(threshold: f64, num_perm: usize) -> Self {
        let (b, r) = optimal_lsh_params(threshold, num_perm);
        let tables = (0..b).map(|_| HashMap::new()).collect();
        Self { b, r, tables }
    }

    fn insert(&mut self, key: &str, mh: &MinHash<'_>) {
        for i in 0..self.b {
            let band: Vec<u64> = mh.hashvalues[i * self.r..(i + 1) * self.r].to_vec();
            self.tables[i]
                .entry(band)
                .or_default()
                .push(key.to_string());
        }
    }

    fn query(&self, mh: &MinHash<'_>) -> HashSet<String> {
        let mut out: HashSet<String> = HashSet::new();
        for i in 0..self.b {
            let band: &[u64] = &mh.hashvalues[i * self.r..(i + 1) * self.r];
            if let Some(bucket) = self.tables[i].get(band) {
                for id in bucket {
                    out.insert(id.clone());
                }
            }
        }
        out
    }
}

// ── normalisation, entropy, shingles ─────────────────────────────────────────

/// Lowercase and collapse runs of non-alphanumeric characters to single
/// spaces, trimmed. Simpler than the Python port's full NFKC pass but
/// sufficient for identifier-style labels.
fn norm(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_sep = true;
    for ch in label.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
            prev_sep = false;
        } else if !prev_sep {
            out.push(' ');
            prev_sep = true;
        }
    }
    out.trim().to_string()
}

/// Shannon entropy in bits per character of the normalised label. Nodes
/// under `ENTROPY_THRESHOLD` are too generic to fuzzy-match reliably.
fn entropy(label: &str) -> f64 {
    let s = norm(label);
    if s.is_empty() {
        return 0.0;
    }
    let mut freq: HashMap<char, usize> = HashMap::new();
    for ch in s.chars() {
        *freq.entry(ch).or_insert(0) += 1;
    }
    let n = s.chars().count() as f64;
    -freq
        .values()
        .map(|&c| {
            let p = c as f64 / n;
            p * p.log2()
        })
        .sum::<f64>()
}

/// k-gram character shingles of `text`. Spaces are stripped so
/// "graph extractor" and "graphextractor" share shingles.
fn shingles(text: &str, k: usize) -> HashSet<String> {
    let stripped: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let chars: Vec<char> = stripped.chars().collect();
    if chars.len() < k {
        return std::iter::once(stripped).collect();
    }
    (0..=chars.len() - k)
        .map(|i| chars[i..i + k].iter().collect::<String>())
        .collect()
}

fn make_minhash<'c>(text: &str, coeffs: &'c MinHashCoeffs) -> MinHash<'c> {
    let mut m = MinHash::new(coeffs);
    for shingle in shingles(text, 3) {
        m.update(shingle.as_bytes());
    }
    m
}

// ── Jaro-Winkler ─────────────────────────────────────────────────────────────

/// Jaro similarity in [0.0, 1.0]. Standard algorithm; operates on characters
/// so it is Unicode-safe.
fn jaro(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let alen = a_chars.len();
    let blen = b_chars.len();
    if alen == 0 || blen == 0 {
        return 0.0;
    }
    let match_dist = alen.max(blen) / 2;
    let match_dist = if match_dist == 0 { 0 } else { match_dist - 1 };
    let mut a_flags = vec![false; alen];
    let mut b_flags = vec![false; blen];
    let mut matches: usize = 0;
    for i in 0..alen {
        let lo = i.saturating_sub(match_dist);
        let hi = (i + match_dist + 1).min(blen);
        for j in lo..hi {
            if !b_flags[j] && a_chars[i] == b_chars[j] {
                a_flags[i] = true;
                b_flags[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }
    let mut k: usize = 0;
    let mut transpositions: usize = 0;
    for i in 0..alen {
        if !a_flags[i] {
            continue;
        }
        while !b_flags[k] {
            k += 1;
        }
        if a_chars[i] != b_chars[k] {
            transpositions += 1;
        }
        k += 1;
    }
    let m = matches as f64;
    (m / alen as f64 + m / blen as f64 + (m - transpositions as f64 / 2.0) / m) / 3.0
}

/// Jaro-Winkler adds a prefix bonus for shared leading characters (up to 4).
/// Match rapidfuzz's scaling: returned value is on [0.0, 100.0].
fn jaro_winkler_pct(a: &str, b: &str) -> f64 {
    let j = jaro(a, b);
    let mut prefix: usize = 0;
    for (ca, cb) in a.chars().zip(b.chars()).take(4) {
        if ca == cb {
            prefix += 1;
        } else {
            break;
        }
    }
    (j + prefix as f64 * 0.1 * (1.0 - j)) * 100.0
}

fn jaro_pct(a: &str, b: &str) -> f64 {
    jaro(a, b) * 100.0
}

// ── merge guards ─────────────────────────────────────────────────────────────

fn is_code(node: &GraphNode) -> bool {
    node.extra
        .get("file_type")
        .and_then(|v| v.as_str())
        .is_some_and(|t| t == "code")
}

fn file_type(node: &GraphNode) -> Option<&str> {
    node.extra.get("file_type").and_then(|v| v.as_str())
}

/// Sibling model/SKU variants — same stem, different trailing digits or
/// letter cluster (M1 / M2, cranel / cranelr). Only applied to short
/// labels; long labels go through Jaro-Winkler normally.
fn is_variant_pair(a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    if a.len().max(b.len()) >= SHORT_LABEL_MAX {
        return false;
    }
    // Split with the longest possible stem ending in a lowercase letter,
    // where the suffix is either a digit-run with optional trailing letters
    // (`m1`, `Cortex-A55`) or two-or-more lowercase letters (`cranelr`).
    // Mirrors the Python regex `^(.*[a-z])([0-9]+[a-z]*|[a-z]{2,})$`.
    fn split(s: &str) -> Option<(String, String)> {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() < 2 {
            return None;
        }
        // Try splits from longest-stem to shortest, keeping the greedy match.
        for stem_len in (1..chars.len()).rev() {
            if !chars[stem_len - 1].is_ascii_lowercase() {
                continue;
            }
            let suffix: String = chars[stem_len..].iter().collect();
            let suffix_ok = {
                let digits = suffix.chars().take_while(|c| c.is_ascii_digit()).count();
                let rest_letters = suffix.chars().skip(digits).all(|c| c.is_ascii_lowercase());
                let all_letters =
                    suffix.chars().all(|c| c.is_ascii_lowercase()) && suffix.chars().count() >= 2;
                (digits > 0 && rest_letters) || all_letters
            };
            if suffix_ok {
                let stem: String = chars[..stem_len].iter().collect();
                return Some((stem, suffix));
            }
        }
        None
    }
    let (sa, ta) = match split(a) {
        Some(x) => x,
        None => return false,
    };
    let (sb, tb) = match split(b) {
        Some(x) => x,
        None => return false,
    };
    sa == sb && ta != tb
}

/// True when two labels carry different embedded numbers, treated as
/// multisets with leading zeros stripped. Numbered/versioned siblings are
/// decisively distinct regardless of Jaro-Winkler score.
fn numeric_tokens_differ(a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    let extract = |s: &str| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut current = String::new();
        for ch in s.chars() {
            if ch.is_ascii_digit() {
                current.push(ch);
            } else if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
        out.into_iter()
            .map(|t| {
                let stripped = t.trim_start_matches('0');
                if stripped.is_empty() {
                    "0".to_string()
                } else {
                    stripped.to_string()
                }
            })
            .collect()
    };
    let mut na = extract(a);
    let mut nb = extract(b);
    na.sort();
    nb.sort();
    na != nb
}

fn short_label_blocked(a: &str, b: &str, jw_score: f64) -> bool {
    if a.len().max(b.len()) >= SHORT_LABEL_MAX {
        return false;
    }
    if jw_score >= 97.0 && a.len() == b.len() && damerau_levenshtein(a, b) <= 1 {
        return false;
    }
    true
}

/// Same-length single-character substitution distance. Small implementation
/// (rows only) since we only compare short strings.
fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    if ac.len() != bc.len() {
        return ac.len().abs_diff(bc.len()) + 1;
    }
    ac.iter().zip(bc.iter()).filter(|(x, y)| x != y).count()
}

fn crossfile_fileanchored_blocked(a: &GraphNode, b: &GraphNode) -> bool {
    let ta = file_type(a);
    let tb = file_type(b);
    let a_anchor = ta.is_some_and(|t| FILE_ANCHORED_NONCODE.contains(&t));
    let b_anchor = tb.is_some_and(|t| FILE_ANCHORED_NONCODE.contains(&t));
    if !a_anchor && !b_anchor {
        return false;
    }
    a.source_file != b.source_file
}

// ── union-find ───────────────────────────────────────────────────────────────

struct UnionFind {
    parent: HashMap<String, String>,
}

impl UnionFind {
    fn new() -> Self {
        Self {
            parent: HashMap::new(),
        }
    }
    fn find(&mut self, x: &str) -> String {
        self.parent
            .entry(x.to_string())
            .or_insert_with(|| x.to_string());
        let mut cur = x.to_string();
        loop {
            let p = self.parent.get(&cur).unwrap().clone();
            if p == cur {
                return cur;
            }
            // Path compression.
            let gp = self.parent.get(&p).cloned().unwrap_or_else(|| p.clone());
            self.parent.insert(cur.clone(), gp.clone());
            cur = gp;
        }
    }
    fn union(&mut self, x: &str, y: &str) {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx != ry {
            self.parent.insert(ry, rx);
        }
    }
    fn components(&mut self) -> HashMap<String, Vec<String>> {
        let keys: Vec<String> = self.parent.keys().cloned().collect();
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        for k in keys {
            let root = self.find(&k);
            groups.entry(root).or_default().push(k);
        }
        groups
    }
}

/// Pick the canonical survivor for a merged group. Preference:
/// 1. Nodes whose id does *not* look like a chunk suffix (`_c\d+$`)
/// 2. Then shortest id
fn pick_winner<'a>(nodes: &'a [&'a GraphNode]) -> &'a GraphNode {
    let has_suffix = |n: &GraphNode| -> bool {
        if let Some(pos) = n.id.rfind(CHUNK_SUFFIX_MARKER) {
            n.id[pos + CHUNK_SUFFIX_MARKER.len()..]
                .chars()
                .all(|c| c.is_ascii_digit())
                && pos + CHUNK_SUFFIX_MARKER.len() < n.id.len()
        } else {
            false
        }
    };
    // Lexicographic id is the third-tie-breaker so the result is stable
    // regardless of HashMap iteration order (which `components()` uses).
    // `min_by_key` returns the *last* element with the minimum key when
    // multiple elements tie, so a pure `(has_suffix, id_len)` key was
    // implicitly order-sensitive.
    nodes
        .iter()
        .min_by(|a, b| {
            (has_suffix(a) as u32, a.id.len(), &a.id).cmp(&(
                has_suffix(b) as u32,
                b.id.len(),
                &b.id,
            ))
        })
        .copied()
        .unwrap()
}

// ── main entry point ─────────────────────────────────────────────────────────

/// Deduplicate near-identical entities across `nodes` / `edges`.
///
/// `communities` maps node ids to their community index (as produced by the
/// cluster step); pairs in the same community get a small score bonus.
///
/// When `dedup_llm_backend` is `Some`, pairs whose score falls in the
/// `[LLM_LOW, LLM_HIGH)` ambiguity window are counted for follow-up. The
/// actual LLM call is a follow-up: the plumbing is in place so a downstream
/// change can turn the count into merges without another API redesign.
pub fn deduplicate_entities(
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    communities: &HashMap<String, usize>,
    dedup_llm_backend: Option<&str>,
) -> (Vec<GraphNode>, Vec<GraphEdge>, DedupStats) {
    let mut stats = DedupStats::default();
    if nodes.len() <= 1 {
        return (nodes, edges, stats);
    }

    // Pass 0: ID pre-dedup. First occurrence wins; log collisions between
    // different source files.
    let mut seen_ids: BTreeMap<String, GraphNode> = BTreeMap::new();
    for node in nodes {
        if let Some(existing) = seen_ids.get(&node.id) {
            if existing.source_file != node.source_file {
                stats.id_collisions += 1;
            }
        } else {
            seen_ids.insert(node.id.clone(), node);
        }
    }
    let unique_nodes: Vec<GraphNode> = seen_ids.into_values().collect();
    if unique_nodes.len() <= 1 {
        return (unique_nodes, edges, stats);
    }

    // ── pass 1: exact normalisation, same-file merges ────────────────────────
    let mut uf = UnionFind::new();
    let mut norm_groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, node) in unique_nodes.iter().enumerate() {
        if is_code(node) {
            continue;
        }
        let key = norm(&node.label);
        if key.is_empty() {
            continue;
        }
        norm_groups.entry(key).or_default().push(i);
    }
    for indices in norm_groups.values() {
        if indices.len() < 2 {
            continue;
        }
        // Partition by source_file — only merge within the same file.
        let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
        for &i in indices {
            let sf = unique_nodes[i].source_file.as_str();
            if sf.is_empty() {
                continue;
            }
            by_file.entry(sf).or_default().push(i);
        }
        for group in by_file.values() {
            if group.len() <= 1 {
                continue;
            }
            let refs: Vec<&GraphNode> = group.iter().map(|&i| &unique_nodes[i]).collect();
            let winner_id = pick_winner(&refs).id.clone();
            for &i in group {
                if unique_nodes[i].id != winner_id {
                    uf.union(&winner_id, &unique_nodes[i].id);
                }
            }
            stats.exact_merges += group.len() - 1;
        }
    }

    // ── pass 2: MinHash + LSH + Jaro-Winkler ─────────────────────────────────
    let coeffs = MinHashCoeffs::new(NUM_PERM);
    let mut candidates_idx: Vec<usize> = Vec::new();
    let mut seen_norms: HashSet<String> = HashSet::new();
    for (i, node) in unique_nodes.iter().enumerate() {
        if is_code(node) {
            continue;
        }
        let key = norm(&node.label);
        if key.is_empty() || !seen_norms.insert(key.clone()) {
            continue;
        }
        if entropy(&node.label) >= ENTROPY_THRESHOLD {
            candidates_idx.push(i);
        }
    }

    if candidates_idx.len() >= 2 {
        let mut lsh = MinHashLsh::new(LSH_THRESHOLD, NUM_PERM);
        let mut minhashes: HashMap<String, MinHash<'_>> = HashMap::new();
        let mut norm_cache: HashMap<String, String> = HashMap::new();
        for &i in &candidates_idx {
            let node = &unique_nodes[i];
            let nl = norm(&node.label);
            let mh = make_minhash(&nl, &coeffs);
            lsh.insert(&node.id, &mh);
            minhashes.insert(node.id.clone(), mh);
            norm_cache.insert(node.id.clone(), nl);
        }

        // Index for O(1) node lookup and ambiguity tracking.
        let id_to_idx: HashMap<&str, usize> = candidates_idx
            .iter()
            .map(|&i| (unique_nodes[i].id.as_str(), i))
            .collect();
        let mut ambiguous_seen: HashSet<(String, String)> = HashSet::new();

        for &i in &candidates_idx {
            let node = &unique_nodes[i];
            let norm_label = &norm_cache[&node.id];
            let mh = &minhashes[&node.id];
            let neighbours = lsh.query(mh);
            for nb_id in neighbours {
                if nb_id == node.id {
                    continue;
                }
                if uf.find(&node.id) == uf.find(&nb_id) {
                    continue;
                }
                let Some(&j) = id_to_idx.get(nb_id.as_str()) else {
                    continue;
                };
                let neighbour = &unique_nodes[j];
                let nb_norm = &norm_cache[&nb_id];

                let xfile = node.source_file != neighbour.source_file;
                let mut score = if xfile && norm_label.len().max(nb_norm.len()) >= SHORT_LABEL_MAX {
                    jaro_pct(norm_label, nb_norm)
                } else {
                    jaro_winkler_pct(norm_label, nb_norm)
                };

                if is_variant_pair(norm_label, nb_norm) {
                    continue;
                }
                if short_label_blocked(norm_label, nb_norm, score) {
                    continue;
                }
                // Prefix-extension pairs (getActiveSession vs getActiveSessions)
                // are almost never duplicates; block regardless of score.
                let (lo, hi) = if norm_label.len() <= nb_norm.len() {
                    (norm_label.as_str(), nb_norm.as_str())
                } else {
                    (nb_norm.as_str(), norm_label.as_str())
                };
                if hi.starts_with(lo) && hi != lo {
                    continue;
                }
                if numeric_tokens_differ(norm_label, nb_norm) {
                    continue;
                }
                if crossfile_fileanchored_blocked(node, neighbour) {
                    continue;
                }

                if let (Some(&c1), Some(&c2)) = (communities.get(&node.id), communities.get(&nb_id))
                    && c1 == c2
                    && norm_label.chars().count().min(nb_norm.chars().count()) >= SHORT_LABEL_MAX
                {
                    score += COMMUNITY_BOOST;
                }

                if score >= MERGE_THRESHOLD {
                    // Identical normalised labels across different source
                    // files almost always mean same-named-but-different
                    // symbols. Mirror Pass 1's per-file guard.
                    if norm_label == nb_norm && xfile {
                        continue;
                    }
                    let refs: [&GraphNode; 2] = [node, neighbour];
                    let winner_id = pick_winner(&refs).id.clone();
                    uf.union(&winner_id, &node.id);
                    uf.union(&winner_id, &nb_id);
                    stats.fuzzy_merges += 1;
                } else if (LLM_LOW..LLM_HIGH).contains(&score) && dedup_llm_backend.is_some() {
                    let key = if node.id < nb_id {
                        (node.id.clone(), nb_id.clone())
                    } else {
                        (nb_id.clone(), node.id.clone())
                    };
                    if ambiguous_seen.insert(key) {
                        stats.ambiguous_pairs += 1;
                    }
                }
            }
        }
    }

    // ── build remap from union-find components ───────────────────────────────
    let mut components = uf.components();
    let mut remap: HashMap<String, String> = HashMap::new();
    let id_to_node: HashMap<&str, &GraphNode> =
        unique_nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    for members in components.values_mut() {
        if members.len() < 2 {
            continue;
        }
        let refs: Vec<&GraphNode> = members
            .iter()
            .filter_map(|id| id_to_node.get(id.as_str()).copied())
            .collect();
        if refs.is_empty() {
            continue;
        }
        let winner_id = pick_winner(&refs).id.clone();
        for m in members {
            if *m != winner_id {
                remap.insert(m.clone(), winner_id.clone());
            }
        }
    }

    if remap.is_empty() {
        return (unique_nodes, edges, stats);
    }

    let survivors: Vec<GraphNode> = unique_nodes
        .into_iter()
        .filter(|n| !remap.contains_key(&n.id))
        .collect();

    let mut rewired: Vec<GraphEdge> = Vec::with_capacity(edges.len());
    for edge in edges {
        let src = remap.get(&edge.source).cloned().unwrap_or(edge.source);
        let tgt = remap.get(&edge.target).cloned().unwrap_or(edge.target);
        if src == tgt {
            continue; // self-loop introduced by a merge; drop.
        }
        rewired.push(GraphEdge {
            source: src,
            target: tgt,
            ..edge
        });
    }

    (survivors, rewired, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::model::{GraphEdge, GraphNode, NodeType};
    use std::collections::HashMap;

    fn node_with(id: &str, label: &str, file: &str, ft: &str) -> GraphNode {
        let mut extra = HashMap::new();
        extra.insert(
            "file_type".to_string(),
            serde_json::Value::String(ft.to_string()),
        );
        GraphNode {
            id: id.to_string(),
            label: label.to_string(),
            source_file: file.to_string(),
            source_location: None,
            node_type: NodeType::Concept,
            community: None,
            extra,
        }
    }
    fn edge(src: &str, tgt: &str) -> GraphEdge {
        GraphEdge {
            source: src.to_string(),
            target: tgt.to_string(),
            relation: "calls".to_string(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: "test".to_string(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn norm_strips_and_lowercases() {
        assert_eq!(norm("  Graph  Extractor "), "graph extractor");
        assert_eq!(norm("Foo::Bar__baz"), "foo bar baz");
    }

    #[test]
    fn entropy_flags_generic_labels() {
        assert!(entropy("aaaaa") < ENTROPY_THRESHOLD);
        assert!(entropy("Graph Extractor") > ENTROPY_THRESHOLD);
    }

    #[test]
    fn shingles_agree_on_stripped_form() {
        let a = shingles("graph extractor", 3);
        let b = shingles("graphextractor", 3);
        assert!(!a.is_disjoint(&b));
        assert!(a.iter().all(|s| !s.contains(' ')));
    }

    #[test]
    fn minhash_signatures_are_deterministic() {
        let c = MinHashCoeffs::new(NUM_PERM);
        let a = make_minhash("graph extractor", &c);
        let b = make_minhash("graph extractor", &c);
        assert_eq!(a.hashvalues, b.hashvalues);
    }

    #[test]
    fn minhash_similar_signatures_overlap() {
        let c = MinHashCoeffs::new(NUM_PERM);
        let a = make_minhash("graph extractor", &c);
        let b = make_minhash("graph extracter", &c); // 1-char diff
        let overlap = a
            .hashvalues
            .iter()
            .zip(b.hashvalues.iter())
            .filter(|(x, y)| x == y)
            .count();
        assert!(
            overlap * 2 >= NUM_PERM,
            "expected majority slot agreement, got {overlap}/{NUM_PERM}"
        );
    }

    #[test]
    fn jaro_winkler_score_matches_reference() {
        // Reference: rapidfuzz("MARTHA","MARHTA") ≈ 96.11
        let s = jaro_winkler_pct("MARTHA", "MARHTA");
        assert!((s - 96.1).abs() < 1.0, "got {s}");
    }

    #[test]
    fn variant_pair_blocks_short_sku_labels() {
        assert!(is_variant_pair("m1", "m2"));
        assert!(is_variant_pair("cortexa55", "cortexa76"));
        // Long labels are not affected.
        assert!(!is_variant_pair(
            "some really long label with numbers 1",
            "some really long label with numbers 2"
        ));
        // Identical labels aren't variants.
        assert!(!is_variant_pair("foo1", "foo1"));
    }

    #[test]
    fn numeric_tokens_differ_blocks_numbered_siblings() {
        assert!(numeric_tokens_differ("ADR 0011 D5", "ADR 0013 D4"));
        assert!(numeric_tokens_differ(
            "3.1 Product Goals",
            "1.1 Product Goals"
        ));
        assert!(!numeric_tokens_differ("no numbers", "no numbers"));
        // Zero-padding is normalised.
        assert!(!numeric_tokens_differ("v09", "v9"));
    }

    #[test]
    fn dedup_merges_exact_same_file_only() {
        let nodes = vec![
            node_with("a1", "Widget", "src/foo.rs", "concept"),
            node_with("a2", "widget", "src/foo.rs", "concept"),
            node_with("b1", "Widget", "src/bar.rs", "concept"),
        ];
        let edges = vec![edge("a2", "b1")];
        let (out_nodes, out_edges, stats) =
            deduplicate_entities(nodes, edges, &HashMap::new(), None);
        assert_eq!(stats.exact_merges, 1, "same file merged");
        assert_eq!(out_nodes.len(), 2, "cross-file same-label kept apart");
        assert!(
            out_edges
                .iter()
                .any(|e| e.source == "a1" && e.target == "b1")
        );
    }

    #[test]
    fn dedup_skips_code_nodes() {
        let nodes = vec![
            node_with("f1", "run", "src/a.rs", "code"),
            node_with("f2", "run", "src/a.rs", "code"),
        ];
        let (out_nodes, _out_edges, stats) =
            deduplicate_entities(nodes, vec![], &HashMap::new(), None);
        assert_eq!(stats.exact_merges, 0);
        assert_eq!(out_nodes.len(), 2);
    }

    #[test]
    fn dedup_fuzzy_merges_concepts_within_file() {
        let nodes = vec![
            node_with("a", "Graph Extractor", "doc.md", "concept"),
            node_with("b", "graph  extractor", "doc.md", "concept"), // exact norm match
        ];
        let (out_nodes, _out_edges, stats) =
            deduplicate_entities(nodes, vec![], &HashMap::new(), None);
        assert_eq!(stats.exact_merges, 1);
        assert_eq!(out_nodes.len(), 1);
    }

    #[test]
    fn dedup_blocks_prefix_extension_pairs() {
        // getActiveSession vs getActiveSessions — cross-file, JW is high but
        // strict prefix — must not merge.
        let nodes = vec![
            node_with("a", "getActiveSession", "x.doc", "concept"),
            node_with("b", "getActiveSessions", "y.doc", "concept"),
        ];
        let (out_nodes, _out_edges, stats) =
            deduplicate_entities(nodes, vec![], &HashMap::new(), None);
        assert_eq!(stats.fuzzy_merges, 0);
        assert_eq!(out_nodes.len(), 2);
    }

    #[test]
    fn dedup_llm_backend_flag_does_not_break_pipeline() {
        // With `dedup_llm_backend = Some(...)` the pipeline must still run
        // to completion and never merge more aggressively than without it;
        // ambiguous pairs are counted only when the flag is on.
        let nodes = vec![
            node_with("a", "Graph Extractor Module", "x.doc", "concept"),
            node_with("b", "Graph Selector Module", "y.doc", "concept"),
            node_with("c", "Unrelated Widget", "z.doc", "concept"),
        ];
        let (out_llm, _, stats_llm) =
            deduplicate_entities(nodes.clone(), vec![], &HashMap::new(), Some("openai"));
        let (out_off, _, stats_off) = deduplicate_entities(nodes, vec![], &HashMap::new(), None);
        assert_eq!(out_llm.len(), out_off.len());
        assert_eq!(stats_off.ambiguous_pairs, 0);
        // Flag doesn't cause extra merges by itself — LLM verdicts are a
        // follow-up. What it may do is count candidate pairs.
        assert!(stats_llm.ambiguous_pairs >= stats_off.ambiguous_pairs);
    }
}
