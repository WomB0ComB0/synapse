//! A small, structural (paragraph/heading-aware) text splitter for ingestion.
//!
//! Chunking is part of the **derived** retrieval pipeline: the canonical
//! document text is the source of truth, and chunks are rebuildable. This
//! splitter is intentionally dependency-free and deterministic so ingestion is
//! reproducible and unit-testable without a database.
//!
//! Strategy:
//! - Break the text into structural blocks: blank-line-separated paragraphs plus
//!   Markdown-style headings (`#`, `##`, …). A heading updates a hierarchical
//!   section-path **stack** (keyed on the heading level) and is emitted as its own
//!   block so it stays searchable; nested headings (`# H1`, `## H2`) accumulate as
//!   `section_path = ["H1", "H2"]`.
//! - Greedily pack blocks into ~[`TARGET_CHARS`] windows (comparing **character**
//!   counts, not bytes), carrying a short [`OVERLAP_CHARS`] tail from the previous
//!   chunk so context spans boundaries.
//! - Record `ordinal`, `char_start`/`char_end` (true **character** offsets of the
//!   chunk's primary span in the original text, excluding the duplicated overlap),
//!   and the hierarchical `section_path` in effect.

/// Approximate maximum size of a chunk's primary span, in characters.
pub const TARGET_CHARS: usize = 800;
/// Number of trailing characters carried from the previous chunk as overlap.
pub const OVERLAP_CHARS: usize = 120;

/// One structural chunk produced by [`split`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkPiece {
    /// Zero-based position within the parent document.
    pub ordinal: i32,
    /// Chunk text, including the overlap prefix carried from the prior chunk.
    pub text: String,
    /// Inclusive character offset of the chunk's primary span in the source text.
    pub char_start: i32,
    /// Exclusive character offset of the chunk's primary span in the source text.
    pub char_end: i32,
    /// Hierarchical section headings locating the chunk (may be empty).
    pub section_path: Vec<String>,
}

/// A structural block (paragraph or heading) with its byte span, character
/// length, and section path. `start`/`end` stay **byte** offsets because they
/// index into `src` for slicing; `char_len` is the block's length in
/// **characters** so the packing budget compares chars-to-chars (not bytes).
struct Block {
    start: usize,
    end: usize,
    char_len: usize,
    text: String,
    section: Vec<String>,
    is_heading: bool,
}

/// Parse a Markdown-style heading, returning `(level, title)` if `line` is one.
///
/// `level` is the number of leading `#` characters (`## Steps` -> level 2),
/// which drives the hierarchical section-path stack. A `#` run must be followed
/// by a space (e.g. `## Steps`), not `#tag`, and the title must be non-empty.
fn parse_heading(line: &str) -> Option<(usize, String)> {
    if !line.starts_with('#') {
        return None;
    }
    let level = line.len() - line.trim_start_matches('#').len();
    let rest = &line[level..];
    // Require the `#` run to be followed by a space, then a non-empty title.
    if !rest.starts_with(' ') {
        return None;
    }
    let title = rest.trim();
    if title.is_empty() {
        return None;
    }
    Some((level, title.to_string()))
}

/// The current hierarchical section path: the titles on the heading stack, from
/// shallowest to deepest (e.g. `[(1,"H1"), (2,"H2")]` -> `["H1", "H2"]`).
fn section_titles(stack: &[(usize, String)]) -> Vec<String> {
    stack.iter().map(|(_, title)| title.clone()).collect()
}

fn push_block(
    blocks: &mut Vec<Block>,
    src: &str,
    start: usize,
    end: usize,
    section: &[String],
    is_heading: bool,
) {
    if end > start {
        let text = src[start..end].to_string();
        let char_len = text.chars().count();
        blocks.push(Block {
            start,
            end,
            char_len,
            text,
            section: section.to_vec(),
            is_heading,
        });
    }
}

/// Split `input` into overlapping, paragraph/heading-aware chunks.
pub fn split(input: &str) -> Vec<ChunkPiece> {
    // --- 1. Structural blocks (paragraphs + headings) with byte spans. ---
    let mut blocks: Vec<Block> = Vec::new();
    // Hierarchical heading stack of (level, title). The current `section_path` is
    // the stack's titles: a heading at level L pops all entries with level >= L
    // (so siblings replace and deeper headings nest) before pushing itself, so
    // `# H1` then `## H2` yields ["H1", "H2"].
    let mut heading_stack: Vec<(usize, String)> = Vec::new();
    let mut para_start: Option<usize> = None;
    let mut para_end = 0usize;
    let mut offset = 0usize;

    for line in input.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim();

        if trimmed.is_empty() {
            if let Some(s) = para_start.take() {
                push_block(
                    &mut blocks,
                    input,
                    s,
                    para_end,
                    &section_titles(&heading_stack),
                    false,
                );
            }
            continue;
        }

        if let Some((level, title)) = parse_heading(trimmed) {
            if let Some(s) = para_start.take() {
                push_block(
                    &mut blocks,
                    input,
                    s,
                    para_end,
                    &section_titles(&heading_stack),
                    false,
                );
            }
            // Nest under shallower headings; replace same-or-deeper ones.
            while heading_stack.last().is_some_and(|(l, _)| *l >= level) {
                heading_stack.pop();
            }
            heading_stack.push((level, title));
            let section = section_titles(&heading_stack);
            let lead_ws = content.len() - content.trim_start().len();
            let h_start = line_start + lead_ws;
            let h_end = line_start + content.trim_end().len();
            push_block(&mut blocks, input, h_start, h_end, &section, true);
            continue;
        }

        // Ordinary line: part of the current paragraph.
        if para_start.is_none() {
            let lead_ws = content.len() - content.trim_start().len();
            para_start = Some(line_start + lead_ws);
        }
        para_end = line_start + content.trim_end().len();
    }
    if let Some(s) = para_start.take() {
        push_block(
            &mut blocks,
            input,
            s,
            para_end,
            &section_titles(&heading_stack),
            false,
        );
    }

    // --- 2. Greedily pack blocks into ~TARGET_CHARS windows with overlap. ---
    let mut pieces: Vec<ChunkPiece> = Vec::new();
    let mut i = 0usize;
    while i < blocks.len() {
        let section_path = blocks[i].section.clone();
        let start = blocks[i].start;
        let mut end = blocks[i].end;
        let mut body = String::new();
        let mut len = 0usize;

        while i < blocks.len() {
            let b = &blocks[i];
            // Compare CHARACTER counts against the char budget (TARGET_CHARS);
            // using byte lengths would split multi-byte text too early.
            let blen = b.char_len;
            // Always take at least one block; otherwise start a fresh chunk at a
            // heading (so section_path tracks the heading) or when the target
            // size would be exceeded.
            if len > 0 && (b.is_heading || len + blen > TARGET_CHARS) {
                break;
            }
            if !body.is_empty() {
                body.push_str("\n\n");
                len += 2;
            }
            body.push_str(&b.text);
            end = b.end;
            len += blen;
            i += 1;
            if len >= TARGET_CHARS {
                break;
            }
        }

        // Prepend a short overlap tail from the previous chunk for continuity.
        let text = match pieces.last() {
            Some(prev) if OVERLAP_CHARS > 0 => {
                let tail = tail_chars(&prev.text, OVERLAP_CHARS);
                if tail.is_empty() {
                    body
                } else {
                    format!("{tail}\n\n{body}")
                }
            }
            _ => body,
        };

        pieces.push(ChunkPiece {
            ordinal: pieces.len() as i32,
            text,
            // Convert the byte span [start, end) to true CHARACTER offsets so the
            // canonical schema's char_start/char_end are accurate for non-ASCII.
            char_start: input[..start].chars().count() as i32,
            char_end: input[..end].chars().count() as i32,
            section_path,
        });
    }

    pieces
}

/// Return the last `n` characters of `s` (respecting char boundaries).
fn tail_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    s.chars().skip(total - n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(split("").is_empty());
        assert!(split("   \n\n  \n").is_empty());
    }

    #[test]
    fn single_paragraph_is_one_chunk_with_offsets() {
        let text = "Hello world, this is a compact paragraph.";
        let pieces = split(text);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].ordinal, 0);
        assert_eq!(pieces[0].char_start, 0);
        assert_eq!(pieces[0].char_end as usize, text.len());
        assert_eq!(
            &text[pieces[0].char_start as usize..pieces[0].char_end as usize],
            text
        );
    }

    #[test]
    fn headings_populate_hierarchical_section_path() {
        let text = "# Incident Response\n\nPromote the standby replica.\n\n## Steps\n\nUpdate the connection string.";
        let pieces = split(text);
        // The H1 section is present on its own.
        assert!(pieces
            .iter()
            .any(|p| p.section_path == vec!["Incident Response".to_string()]));
        // The nested H2 keeps its ancestor: ["Incident Response", "Steps"], NOT ["Steps"].
        assert!(
            pieces
                .iter()
                .any(|p| p.section_path
                    == vec!["Incident Response".to_string(), "Steps".to_string()]),
            "nested heading must retain its parent in section_path: {:?}",
            pieces.iter().map(|p| &p.section_path).collect::<Vec<_>>()
        );
        // The flattened single-title path must NOT appear for the nested heading.
        assert!(!pieces
            .iter()
            .any(|p| p.section_path == vec!["Steps".to_string()]));
    }

    #[test]
    fn sibling_and_deeper_headings_maintain_stack() {
        // A(1) > B(2), sibling C(2) replaces B, then D(1) resets the whole stack.
        let text = "# A\n\nalpha\n\n## B\n\nbeta\n\n## C\n\ngamma\n\n# D\n\ndelta";
        let pieces = split(text);
        let paths: Vec<&Vec<String>> = pieces.iter().map(|p| &p.section_path).collect();
        assert!(paths.iter().any(|p| **p == vec!["A".to_string()]));
        assert!(paths
            .iter()
            .any(|p| **p == vec!["A".to_string(), "B".to_string()]));
        assert!(paths
            .iter()
            .any(|p| **p == vec!["A".to_string(), "C".to_string()]));
        assert!(paths.iter().any(|p| **p == vec!["D".to_string()]));
        // C must not nest under B, and D must reset the stack (no leftover C).
        assert!(!paths
            .iter()
            .any(|p| **p == vec!["A".to_string(), "B".to_string(), "C".to_string()]));
        assert!(!paths
            .iter()
            .any(|p| **p == vec!["D".to_string(), "C".to_string()]));
    }

    #[test]
    fn char_offsets_are_characters_not_bytes() {
        // Multi-byte content: char count is strictly less than the byte length.
        let text = "café ☕ résumé is short.";
        let pieces = split(text);
        assert_eq!(pieces.len(), 1);
        let p = &pieces[0];
        assert_eq!(p.char_start, 0);
        assert_eq!(
            p.char_end as usize,
            text.chars().count(),
            "char_end must be a CHARACTER count"
        );
        assert!(
            (p.char_end as usize) < text.len(),
            "char_end must be a character offset (< byte length for multi-byte text): \
             char_end={}, bytes={}",
            p.char_end,
            text.len()
        );
    }

    #[test]
    fn long_text_splits_into_multiple_overlapping_chunks() {
        // 6 paragraphs of ~300 chars each => must exceed the 800-char target.
        let para = "x".repeat(300);
        let text = vec![para; 6].join("\n\n");
        let pieces = split(&text);
        assert!(
            pieces.len() >= 2,
            "expected multiple chunks, got {}",
            pieces.len()
        );
        for (i, p) in pieces.iter().enumerate() {
            assert_eq!(p.ordinal as usize, i);
        }
        for p in &pieces {
            assert!(p.char_start <= p.char_end);
            assert!(p.char_end as usize <= text.len());
        }
    }
}
