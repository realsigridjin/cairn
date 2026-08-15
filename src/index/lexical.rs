use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Posting {
    pub doc_idx: u32,
    pub tf: u16,
    pub doc_len: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostingList {
    pub df: u32,
    pub postings: Vec<Posting>,
    pub max_tf: u16,
    pub min_doc_len: u32,
}

pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || is_cjk(ch) {
            for lower in ch.to_lowercase() {
                current.push(lower);
            }
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }

    // Mirror the broad shape of UQA-RS standard_cjk with 2-to-3-character
    // n-grams. This is still intentionally small; production parity must be
    // measured against the exact analyzer configuration in the warm UQA DB.
    let mut extra = Vec::new();
    for token in &out {
        let chars: Vec<char> = token.chars().collect();
        if chars.iter().any(|c| is_cjk(*c)) {
            for n in [2usize, 3usize] {
                if chars.len() >= n {
                    for gram in chars.windows(n) {
                        extra.push(gram.iter().collect());
                    }
                }
            }
        }
    }
    out.extend(extra);
    out
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF |
        0x3040..=0x30FF | 0x31F0..=0x31FF)
}

pub fn build_postings(texts: &[String]) -> Result<(BTreeMap<String, PostingList>, f32, Vec<u32>)> {
    build_postings_iter(texts.iter().map(String::as_str))
}

pub fn build_postings_iter<'a>(
    texts: impl IntoIterator<Item = &'a str>,
) -> Result<(BTreeMap<String, PostingList>, f32, Vec<u32>)> {
    let mut acc: BTreeMap<String, Vec<Posting>> = BTreeMap::new();
    let mut lens = Vec::new();
    for (doc_idx, text) in texts.into_iter().enumerate() {
        let toks = tokenize(text);
        let doc_len =
            u32::try_from(toks.len().max(1)).context("document token count exceeds u32")?;
        lens.push(doc_len);
        let mut tf: BTreeMap<String, u16> = BTreeMap::new();
        for token in toks {
            let entry = tf.entry(token).or_default();
            *entry = entry.checked_add(1).context("term frequency exceeds u16")?;
        }
        let doc_idx = u32::try_from(doc_idx).context("document index exceeds u32")?;
        for (term, count) in tf {
            acc.entry(term).or_default().push(Posting {
                doc_idx,
                tf: count,
                doc_len,
            });
        }
    }
    let avg = if lens.is_empty() {
        1.0
    } else {
        lens.iter().map(|x| *x as f32).sum::<f32>() / lens.len() as f32
    };
    let lists = acc
        .into_iter()
        .map(|(term, postings)| {
            let max_tf = postings.iter().map(|p| p.tf).max().unwrap_or(0);
            let min_doc_len = postings.iter().map(|p| p.doc_len).min().unwrap_or(1);
            let df = u32::try_from(postings.len()).context("posting-list df exceeds u32")?;
            Ok((
                term,
                PostingList {
                    df,
                    postings,
                    max_tf,
                    min_doc_len,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok((lists, avg, lens))
}

pub fn query_terms(query: &str) -> BTreeSet<String> {
    tokenize(query).into_iter().collect()
}

pub fn bm25(tf: u16, doc_len: u32, df: u32, n_docs: u32, avgdl: f32, k1: f32, b: f32) -> f32 {
    let tf = tf as f32;
    let dl = doc_len as f32;
    let n = n_docs.max(1) as f32;
    let df = df.max(1) as f32;
    let idf = (((n - df + 0.5) / (df + 0.5)) + 1.0).ln();
    let denom = tf + k1 * (1.0 - b + b * dl / avgdl.max(1e-6));
    idf * tf * (k1 + 1.0) / denom.max(1e-6)
}
