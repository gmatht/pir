use std::collections::{HashMap, HashSet};

/// Compute spans (overflow/merged) from a pure model of rows.
///
/// rows: slice of rows, each row is a Vec of (widget_key, text).
/// per_cell_px: width in pixels of one cell.
/// measure: closure that given widget_key and text returns the measured pixel width.
pub fn compute_spans_from_model<F>(rows: &[Vec<(usize, String)>], per_cell_px: i32, mut measure: F) -> Vec<(usize, usize, usize, String)>
where
    F: FnMut(usize, &str) -> i32,
{
    let mut spans: Vec<(usize, usize, usize, String)> = Vec::new();

    for (r_idx, row) in rows.iter().enumerate() {
        let cols = row.len();
        if cols == 0 { continue; }

        // Pointer/identity vector
        let ptrs: Vec<usize> = row.iter().map(|(p, _)| *p).collect();

        // Determine unique starts and coverage (how many consecutive columns share the same widget)
        let mut seen: HashSet<usize> = HashSet::new();
        let mut starts: Vec<bool> = vec![false; cols];
        let mut coverage: Vec<usize> = vec![1; cols];
        let mut i = 0;
        while i < cols {
            let key = ptrs[i];
            if !seen.contains(&key) {
                starts[i] = true;
                seen.insert(key);
                // count consecutive same pointers
                let mut c = 1usize;
                let mut k = i + 1;
                while k < cols && ptrs[k] == key { c += 1; k += 1; }
                coverage[i] = c;
                i = k;
            } else {
                starts[i] = false;
                i += 1;
            }
        }

        // Original texts for each position (we only care about start positions)
        let orig_texts: Vec<String> = row.iter().map(|(_, t)| t.clone()).collect();

        // Measurement cache to avoid repeated measurements
        let mut measure_cache: HashMap<(usize, String), i32> = HashMap::new();

        // Iterate over logical columns, but only handle start positions (unique widgets)
        let mut i = 0;
        while i < cols {
            if !starts[i] { i += 1; continue; }
            let s = orig_texts[i].clone();
            if s.is_empty() { i += 1; continue; }

            let widget_key = ptrs[i];
            let cov = coverage[i];

            // If this widget spans multiple columns (explicit merged cell), treat it as a single large cell
            if cov > 1 {
                spans.push((r_idx, i, cov, s));
                i += cov;
                continue;
            }

            let full_key = (widget_key, s.clone());
            let measured_full = if let Some(&v) = measure_cache.get(&full_key) {
                v
            } else {
                let m = measure(widget_key, &s);
                measure_cache.insert(full_key.clone(), m);
                m
            };
            if measured_full <= per_cell_px { i += 1; continue; }

            // collect target start indices (i plus following empty starts)
            let mut targets: Vec<usize> = Vec::new();
            let mut j = i;
            while j < cols && (j == i || (orig_texts[j].is_empty() && starts[j])) { targets.push(j); j += 1; }

            // Only create a span if we have more than one target (i.e., can spill into adjacent empty cells)
            if targets.len() > 1 {
                spans.push((r_idx, i, targets.len(), s));
            }

            i = j;
        }
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::compute_spans_from_model;

    #[test]
    fn long_text_overflows_into_empty_cells() {
        // widget keys are distinct (1,2,3)
        let rows = vec![vec![ (1, "VeryLong".to_string()), (2, "".to_string()), (3, "".to_string()) ]];
        // measure: approximate by character count * 10
        let measure = |_: usize, s: &str| -> i32 { (s.len() as i32) * 10 };
        let spans = compute_spans_from_model(&rows, 20, measure);
        // Expect a single span starting at col 0 spanning 3 cells
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0], (0, 0, 3, "VeryLong".to_string()));
    }

    #[test]
    fn does_not_overflow_when_next_cell_used() {
        let rows = vec![vec![ (1, "VeryLong".to_string()), (2, "X".to_string()), (3, "".to_string()) ]];
        let measure = |_: usize, s: &str| -> i32 { (s.len() as i32) * 10 };
        let spans = compute_spans_from_model(&rows, 20, measure);
        // Next cell is non-empty so no overflow span
        assert!(spans.is_empty());
    }

    #[test]
    fn merged_cells_yield_coverage_span() {
        // widget 1 spans columns 0 and 1
        let rows = vec![vec![ (1, "Merged".to_string()), (1, "Merged".to_string()), (3, "".to_string()) ]];
        let measure = |_: usize, s: &str| -> i32 { (s.len() as i32) * 10 };
        let spans = compute_spans_from_model(&rows, 1200, measure);
        // coverage span of length 2 at start column 0
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0], (0, 0, 2, "Merged".to_string()));
    }
}
