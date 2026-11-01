//! A small WASI program for trying out wasm-dap: counts words given as
//! arguments (or a built-in sentence) and prints the most frequent ones.

use std::collections::HashMap;
use std::env;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Category {
    Short,
    Medium,
    Long,
}

#[derive(Debug)]
struct WordStats {
    word: String,
    count: usize,
    category: Category,
}

fn categorize(word: &str) -> Category {
    match word.len() {
        0..=3 => Category::Short,
        4..=7 => Category::Medium,
        _ => Category::Long,
    }
}

fn count_words(text: &str) -> Vec<WordStats> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for word in text.split_whitespace() {
        let normalized = word
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if normalized.is_empty() {
            continue;
        }
        *counts.entry(normalized).or_insert(0) += 1;
    }
    let mut stats: Vec<WordStats> = counts
        .into_iter()
        .map(|(word, count)| {
            let category = categorize(&word);
            WordStats {
                word,
                count,
                category,
            }
        })
        .collect();
    stats.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.word.cmp(&b.word)));
    stats
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let text = if args.is_empty() {
        "the quick brown fox jumps over the lazy dog, the end".to_string()
    } else {
        args.join(" ")
    };
    let stats = count_words(&text);
    for entry in stats.iter().take(5) {
        println!("{:<10} {:>3}  {:?}", entry.word, entry.count, entry.category);
    }
    let long_words = stats
        .iter()
        .filter(|s| s.category == Category::Long)
        .count();
    println!("{} distinct words, {} long", stats.len(), long_words);
}
